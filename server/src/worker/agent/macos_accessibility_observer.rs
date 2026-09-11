//! Bounded macOS Accessibility observation for the active Aqua session.

use desk_agent_protocol::computer_use::UiInspectScope;
use std::ffi::{CStr, CString, c_char, c_void};
use std::time::{Duration, Instant};

use super::native_ui_identity::{IdentityStore, NativeElement};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use desk_agent_protocol::computer_use::{UiSemanticAction, UiSemanticActionKind};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use std::cell::RefCell;

use super::computer_use_broker::{
    CollectedUiNode, CollectedUiTree, ObservedApplication, ObservedDesktop,
};

type CfTypeRef = *const c_void;
type CfStringRef = *const c_void;
type CfArrayRef = *const c_void;
type CfTypeId = usize;
type AxUiElementRef = *const c_void;

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const AX_SUCCESS: i32 = 0;
const HARD_DEADLINE: Duration = Duration::from_secs(2);
const AX_MESSAGE_TIMEOUT_SECONDS: f32 = 0.1;
// Traversal uses short per-node reads; actions may open a popover or run an
// application handler. Reusing the traversal timeout can report an unknown
// outcome after a successful click. Never retry a mutation on timeout.
const AX_ACTION_TIMEOUT_SECONDS: f32 = 3.0;
const MAX_STRING_BYTES: usize = 16 * 1024;
const OBJECT_REF_BUDGET: usize = 320;
const CF_NUMBER_SINT64_TYPE: i32 = 4;
const ACTION_OBSERVATION_MAX_DEPTH: u16 = 16;
const ACTION_OBSERVATION_MAX_NODES: u32 = 1_024;
const ACTION_OBSERVATION_MAX_BYTES: u32 = 1024 * 1024;
const AX_VALUE_CGPOINT_TYPE: i32 = 1;
const AX_VALUE_CGSIZE_TYPE: i32 = 2;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: libc::pid_t) -> AxUiElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AxUiElementRef,
        attribute: CfStringRef,
        value: *mut CfTypeRef,
    ) -> i32;
    fn AXUIElementCopyActionNames(element: AxUiElementRef, names: *mut CfArrayRef) -> i32;
    fn AXUIElementIsAttributeSettable(
        element: AxUiElementRef,
        attribute: CfStringRef,
        settable: *mut bool,
    ) -> i32;
    fn AXUIElementPerformAction(element: AxUiElementRef, action: CfStringRef) -> i32;
    fn AXUIElementSetAttributeValue(
        element: AxUiElementRef,
        attribute: CfStringRef,
        value: CfTypeRef,
    ) -> i32;
    fn AXUIElementSetMessagingTimeout(element: AxUiElementRef, timeout_seconds: f32) -> i32;
    fn AXValueGetType(value: CfTypeRef) -> i32;
    fn AXValueGetValue(value: CfTypeRef, value_type: i32, value_ptr: *mut c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: CfTypeRef);
    fn CFEqual(a: CfTypeRef, b: CfTypeRef) -> u8;
    fn CFHash(value: CfTypeRef) -> usize;
    fn CFRetain(value: CfTypeRef) -> CfTypeRef;
    fn CFGetTypeID(value: CfTypeRef) -> CfTypeId;
    fn CFStringGetTypeID() -> CfTypeId;
    fn CFDateGetTypeID() -> CfTypeId;
    fn CFDateGetAbsoluteTime(value: CfTypeRef) -> f64;
    fn CFDateCreate(allocator: CfTypeRef, at: f64) -> CfTypeRef;
    fn CFNumberCreate(allocator: CfTypeRef, number_type: i32, value: *const c_void) -> CfTypeRef;
    fn CFBooleanGetTypeID() -> CfTypeId;
    fn CFBooleanGetValue(value: CfTypeRef) -> bool;
    fn CFNumberGetTypeID() -> CfTypeId;
    fn CFNumberGetValue(number: CfTypeRef, number_type: i32, value: *mut c_void) -> bool;
    static kCFBooleanTrue: CfTypeRef;
    static kCFBooleanFalse: CfTypeRef;
    fn CFStringCreateWithCString(
        allocator: CfTypeRef,
        value: *const c_char,
        encoding: u32,
    ) -> CfStringRef;
    fn CFStringGetLength(value: CfStringRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        value: CfStringRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    fn CFArrayGetCount(array: CfArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfArrayRef, index: isize) -> CfTypeRef;
}

struct OwnedCf(CfTypeRef);

impl Clone for OwnedCf {
    fn clone(&self) -> Self {
        Self(unsafe { CFRetain(self.0) })
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: every `OwnedCf` wraps a Create/Copy-rule object.
            unsafe { CFRelease(self.0) };
        }
    }
}

#[derive(Default)]
struct WalkState {
    found_menu_window: bool,
    visited: usize,
    encoded_bytes: usize,
    truncated: bool,
}

struct WalkConfig {
    query: Option<desk_agent_protocol::computer_use::UiInspectQuery>,
    element_only: bool,
    menu_window: Option<String>,
    scope: UiInspectScope,
    process_id: u32,
    process_started_at: u64,
    max_depth: u16,
    max_nodes: usize,
    max_bytes: usize,
    deadline: Instant,
}

pub(super) struct AppliedUiAction {
    pub(super) changed: bool,
    pub(super) verified: bool,
    pub(super) summary: String,
}

pub(super) fn observe_interactive_desktop() -> Result<ObservedDesktop, AgentError> {
    let foreground_application = frontmost_application()?;
    Ok(ObservedDesktop {
        session_id: unsafe { libc::geteuid() },
        foreground_application: Some(foreground_application),
    })
}

pub(super) fn collect_foreground(
    expected_process_id: u32,
    expected_image_path: &str,
    max_depth: u16,
    max_nodes: u32,
    max_bytes: u32,
) -> Result<CollectedUiTree, AgentError> {
    let current = frontmost_application()?;
    if current.process_id != expected_process_id || current.image_path != expected_image_path {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the foreground application changed during inspection",
            true,
        ));
    }
    collect_application(
        expected_process_id,
        expected_image_path,
        max_depth,
        max_nodes,
        max_bytes,
    )
}

pub(super) fn collect_application(
    expected_process_id: u32,
    expected_image_path: &str,
    max_depth: u16,
    max_nodes: u32,
    max_bytes: u32,
) -> Result<CollectedUiTree, AgentError> {
    collect_application_with_scope(
        expected_process_id,
        expected_image_path,
        max_depth,
        max_nodes,
        max_bytes,
        UiInspectScope::Content,
    )
}

pub(super) fn collect_application_with_scope(
    expected_process_id: u32,
    expected_image_path: &str,
    max_depth: u16,
    max_nodes: u32,
    max_bytes: u32,
    scope: UiInspectScope,
) -> Result<CollectedUiTree, AgentError> {
    collect_application_with_window_scope(
        expected_process_id,
        expected_image_path,
        max_depth,
        max_nodes,
        max_bytes,
        scope,
        None,
    )
}

pub(super) fn collect_application_with_window_scope(
    expected_process_id: u32,
    expected_image_path: &str,
    max_depth: u16,
    max_nodes: u32,
    max_bytes: u32,
    scope: UiInspectScope,
    menu_window: Option<&str>,
) -> Result<CollectedUiTree, AgentError> {
    collect_application_selection(
        expected_process_id,
        expected_image_path,
        max_depth,
        max_nodes,
        max_bytes,
        scope,
        menu_window,
        None,
        false,
    )
}

pub(super) fn collect_application_selection(
    expected_process_id: u32,
    expected_image_path: &str,
    max_depth: u16,
    max_nodes: u32,
    max_bytes: u32,
    scope: UiInspectScope,
    menu_window: Option<&str>,
    query: Option<&desk_agent_protocol::computer_use::UiInspectQuery>,
    element_only: bool,
) -> Result<CollectedUiTree, AgentError> {
    let expected_image_path = expected_image_path.to_owned();
    let query = query.cloned();
    let selection = menu_window.map(str::to_owned);
    super::native_ui_identity::run(move || {
        collect_application_selection_inner(
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

fn collect_application_selection_inner(
    expected_process_id: u32,
    expected_image_path: &str,
    max_depth: u16,
    max_nodes: u32,
    max_bytes: u32,
    scope: UiInspectScope,
    menu_window: Option<&str>,
    query: Option<&desk_agent_protocol::computer_use::UiInspectQuery>,
    element_only: bool,
) -> Result<CollectedUiTree, AgentError> {
    if !crate::macos_permissions::probe().accessibility {
        return Err(failure(
            AgentErrorKind::PermissionDenied,
            "macOS Accessibility permission is required for semantic UI inspection",
            false,
        ));
    }
    let current = application_by_pid(expected_process_id)?;
    if current.process_id != expected_process_id || current.image_path != expected_image_path {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the selected application changed during Accessibility inspection",
            true,
        ));
    }
    let root = unsafe { AXUIElementCreateApplication(expected_process_id as libc::pid_t) };
    if root.is_null() {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the selected application has no Accessibility root",
            true,
        ));
    }
    let root = OwnedCf(root);
    let root = if let Some(id) = menu_window {
        retained_element(id, expected_process_id, process_start(expected_process_id)?)?
    } else {
        root
    };
    set_messaging_timeout(root.0)?;
    let config = WalkConfig {
        query: query.cloned(),
        element_only,
        menu_window: menu_window.map(str::to_owned),
        scope,
        process_id: expected_process_id,
        process_started_at: process_start(expected_process_id)?,
        max_depth,
        max_nodes: max_nodes as usize,
        max_bytes: max_bytes as usize,
        deadline: Instant::now() + HARD_DEADLINE,
    };
    let mut state = WalkState::default();
    let mut nodes = Vec::new();
    walk(
        root.0, None, 0, 0, false, false, &config, &mut state, &mut nodes,
    )?;
    if menu_window.is_some() && !state.found_menu_window {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the selected UI root was not found within the bounded search",
            true,
        ));
    }
    if application_by_pid(expected_process_id)?.process_started_at != current.process_started_at {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the selected application restarted during inspection",
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
        ACTION_OBSERVATION_MAX_DEPTH,
        ACTION_OBSERVATION_MAX_NODES,
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
    let element =
        locate_action_target(expected_process_id, expected_image_path, target_fingerprint)?;
    validate_action_target(element.0, action)
}

pub(super) fn resolve_window_capture_target(
    expected_process_id: u32,
    expected_image_path: &str,
    target_fingerprint: &str,
) -> Result<super::collectors::screen_capture::WindowCaptureTarget, AgentError> {
    let expected_image_path = expected_image_path.to_owned();
    let target_fingerprint = target_fingerprint.to_owned();
    super::native_ui_identity::run(move || {
        resolve_window_capture_target_inner(
            expected_process_id,
            &expected_image_path,
            &target_fingerprint,
        )
    })
}

fn resolve_window_capture_target_inner(
    expected_process_id: u32,
    expected_image_path: &str,
    target_fingerprint: &str,
) -> Result<super::collectors::screen_capture::WindowCaptureTarget, AgentError> {
    let inspected = collect_application_selection(
        expected_process_id,
        expected_image_path,
        16,
        1024,
        1024 * 1024,
        UiInspectScope::All,
        Some(target_fingerprint),
        None,
        false,
    )?;
    if inspected.truncated || inspected.nodes.iter().any(|node| node.is_protected) {
        return Err(failure(
            AgentErrorKind::PermissionDenied,
            "window capture cannot establish that the selected window has no protected fields",
            false,
        ));
    }
    let element =
        locate_action_target(expected_process_id, expected_image_path, target_fingerprint)?;
    let role = attribute_string(element.0, "AXRole").unwrap_or_default();
    if role != "AXWindow" {
        return Err(failure(
            AgentErrorKind::InvalidInput,
            "the owner-selected Accessibility reference is no longer a window",
            false,
        ));
    }
    if attribute_bool(element.0, "AXMinimized").unwrap_or(false) {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the selected window is minimized; restore it before requesting a fresh screenshot",
            true,
        ));
    }
    let title = attribute_string(element.0, "AXTitle").unwrap_or_default();
    let position = attribute_point(element.0, "AXPosition").ok_or_else(|| {
        failure(
            AgentErrorKind::SessionUnavailable,
            "the selected window no longer exposes a capture position",
            true,
        )
    })?;
    let size = attribute_size(element.0, "AXSize").ok_or_else(|| {
        failure(
            AgentErrorKind::SessionUnavailable,
            "the selected window no longer exposes a capture size",
            true,
        )
    })?;
    if !position.x.is_finite()
        || !position.y.is_finite()
        || !size.width.is_finite()
        || !size.height.is_finite()
        || size.width <= 0.0
        || size.height <= 0.0
    {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the selected window has invalid capture bounds",
            true,
        ));
    }
    Ok(super::collectors::screen_capture::WindowCaptureTarget {
        process_id: expected_process_id,
        title,
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    })
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
    let element =
        locate_action_target(expected_process_id, expected_image_path, target_fingerprint)?;
    validate_action_target(element.0, action)?;
    match action {
        UiSemanticAction::Invoke => {
            let names = action_names(element.0);
            let name = if names.iter().any(|name| name == "AXPress") {
                "AXPress"
            } else {
                "AXConfirm"
            };
            perform_action(element.0, name)?;
            Ok(AppliedUiAction::accepted(true))
        }
        UiSemanticAction::Select => {
            set_bool_attribute(element.0, "AXSelected", true)?;
            Ok(AppliedUiAction::accepted(true))
        }
        UiSemanticAction::Toggle { desired } => {
            let before = attribute_toggle_state(element.0).ok_or_else(|| {
                failure(
                    AgentErrorKind::UnsupportedCapability,
                    "the Accessibility target has no boolean toggle state",
                    false,
                )
            })?;
            if before != *desired {
                perform_action(element.0, "AXPress")?;
            }
            Ok(AppliedUiAction::accepted(before != *desired))
        }
        UiSemanticAction::Focus => {
            set_bool_attribute(element.0, "AXFocused", true)?;
            Ok(AppliedUiAction::accepted(true))
        }
        UiSemanticAction::SetValue { value } => {
            if value.len() > MAX_STRING_BYTES {
                return Err(failure(
                    AgentErrorKind::OutputLimitExceeded,
                    "the Accessibility value exceeds its bounded action ceiling",
                    false,
                ));
            }
            let current = copy_attribute(element.0, "AXValue").ok_or_else(|| {
                failure(AgentErrorKind::InvalidInput,
                    "Cannot read the native AXValue type; inspect the current UI before choosing another action. No write was attempted.", false)
            })?;
            let value_ref = encode_native_value(current.0, value)?;
            set_attribute(element.0, "AXValue", value_ref.0)?;
            Ok(AppliedUiAction::accepted(true))
        }
        UiSemanticAction::Scroll { .. } => Err(failure(
            AgentErrorKind::UnsupportedCapability,
            "this Accessibility semantic action is not enabled by the macOS adapter",
            false,
        )),
    }
}

fn locate_action_target(
    expected_process_id: u32,
    expected_image_path: &str,
    target_fingerprint: &str,
) -> Result<OwnedCf, AgentError> {
    if !crate::macos_permissions::probe().accessibility {
        return Err(failure(
            AgentErrorKind::PermissionDenied,
            "macOS Accessibility permission is required for semantic UI actions",
            false,
        ));
    }
    let current = application_by_pid(expected_process_id)?;
    if current.process_id != expected_process_id || current.image_path != expected_image_path {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the selected application changed before the Accessibility action",
            false,
        ));
    }
    let element = retained_element(
        target_fingerprint,
        expected_process_id,
        process_start(expected_process_id)?,
    )?;
    set_messaging_timeout(element.0)?;
    Ok(element)
}

fn validate_action_target(
    element: AxUiElementRef,
    action: &UiSemanticAction,
) -> Result<(), AgentError> {
    let subrole = attribute_string(element, "AXSubrole").unwrap_or_default();
    if subrole == "AXSecureTextField" {
        return Err(failure(
            AgentErrorKind::PermissionDenied,
            "secure Accessibility fields cannot receive semantic actions",
            false,
        ));
    }
    if !attribute_bool(element, "AXEnabled").unwrap_or(false) {
        return Err(failure(
            AgentErrorKind::InvalidInput,
            "the Accessibility target is disabled",
            false,
        ));
    }
    let supported = match action {
        UiSemanticAction::Invoke => action_names(element)
            .iter()
            .any(|name| name == "AXPress" || name == "AXConfirm"),
        UiSemanticAction::Select => attribute_settable(element, "AXSelected"),
        UiSemanticAction::SetValue { .. } => {
            attribute_settable(element, "AXValue")
                && copy_attribute(element, "AXValue")
                    .is_some_and(|value| native_value_supported(value.0))
        }
        UiSemanticAction::Focus => attribute_settable(element, "AXFocused"),
        UiSemanticAction::Toggle { .. } => {
            action_names(element).iter().any(|name| name == "AXPress")
                && attribute_toggle_state(element).is_some()
        }
        UiSemanticAction::Scroll { .. } => false,
    };
    if supported {
        Ok(())
    } else {
        Err(failure(
            AgentErrorKind::UnsupportedCapability,
            "the Accessibility target does not support the requested semantic action",
            false,
        ))
    }
}

fn is_menu_role(role: &str) -> bool {
    matches!(
        role,
        "AXMenuBar" | "AXMenuBarItem" | "AXMenu" | "AXMenuItem"
    )
}

fn element_identity(element: AxUiElementRef, config: &WalkConfig) -> Result<String, AgentError> {
    let retained = OwnedCf(unsafe { CFRetain(element) });
    let key = unsafe { CFHash(element) }.to_le_bytes().to_vec();
    IDENTITIES.with(|store| {
        store
            .borrow_mut()
            .identify(config.process_id, config.process_started_at, key, retained)
    })
}

fn walk(
    element: AxUiElementRef,
    parent: Option<(Option<u32>, String)>,
    depth: u16,
    _sibling_ordinal: usize,
    inside_menu: bool,
    within_window: bool,
    config: &WalkConfig,
    state: &mut WalkState,
    output: &mut Vec<CollectedUiNode>,
) -> Result<(), AgentError> {
    if config.element_only && state.found_menu_window {
        return Ok(());
    }
    if state.visited >= 4096 || Instant::now() >= config.deadline {
        state.truncated = true;
        return Ok(());
    }
    state.visited += 1;
    if unsafe { AXUIElementSetMessagingTimeout(element, AX_MESSAGE_TIMEOUT_SECONDS) } != AX_SUCCESS
    {
        state.truncated = true;
        return Ok(());
    }

    let inside_menu =
        inside_menu || attribute_string(element, "AXRole").is_some_and(|role| is_menu_role(&role));
    if config.scope == UiInspectScope::Content && inside_menu {
        return Ok(());
    }
    let identity = element_identity(element, config)?;
    let within_window = within_window
        || config
            .menu_window
            .as_ref()
            .is_some_and(|target| target == &identity);
    state.found_menu_window |= within_window;
    let selected = config.menu_window.is_none() || within_window;
    let emit = selected && (config.scope != UiInspectScope::Menus || inside_menu);
    if output.len() >= config.max_nodes || Instant::now() >= config.deadline {
        state.truncated = true;
        return Ok(());
    }
    let (index, fingerprint) = if emit {
        let (node, strings_truncated) = read_node(
            element,
            identity.clone(),
            parent.as_ref().and_then(|(index, _)| *index),
        );
        let matches = super::computer_use_broker::ui_query_matches(config.query.as_ref(), &node);
        if !matches {
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
    let Some(children) = copy_attribute(element, "AXChildren") else {
        return Ok(());
    };
    let count = unsafe { CFArrayGetCount(children.0) }.max(0) as usize;
    if depth >= config.max_depth {
        state.truncated |= count > 0;
        return Ok(());
    }
    for ordinal in 0..count {
        if config.element_only && state.found_menu_window {
            return Ok(());
        }
        if output.len() >= config.max_nodes || Instant::now() >= config.deadline {
            state.truncated = true;
            return Ok(());
        }
        let child = unsafe { CFArrayGetValueAtIndex(children.0, ordinal as isize) };
        if child.is_null() {
            continue;
        }
        walk(
            child,
            Some((index, fingerprint.clone())),
            depth + 1,
            ordinal,
            inside_menu,
            within_window,
            config,
            state,
            output,
        )?;
    }
    Ok(())
}

fn read_node(
    element: AxUiElementRef,
    fingerprint: String,
    parent_index: Option<u32>,
) -> (CollectedUiNode, bool) {
    let role = attribute_string(element, "AXRole").unwrap_or_else(|| "AXUnknown".into());
    let subrole = attribute_string(element, "AXSubrole").unwrap_or_default();
    let identifier = attribute_string(element, "AXIdentifier").unwrap_or_default();
    let is_protected = subrole == "AXSecureTextField";
    let enabled = attribute_bool(element, "AXEnabled").unwrap_or(false);
    let (role, role_truncated) = bounded_string(if subrole.is_empty() {
        role
    } else {
        format!("{role}/{subrole}")
    });
    let (name, name_truncated) = if is_protected {
        (None, false)
    } else {
        let raw = attribute_string(element, "AXTitle")
            .filter(|text| !text.trim().is_empty())
            .or_else(|| {
                attribute_string(element, "AXDescription").filter(|text| !text.trim().is_empty())
            })
            .or_else(|| {
                let label = copy_attribute(element, "AXTitleUIElement")?;
                attribute_string(label.0, "AXValue")
                    .filter(|text| !text.trim().is_empty())
                    .or_else(|| {
                        attribute_string(label.0, "AXTitle").filter(|text| !text.trim().is_empty())
                    })
            })
            .unwrap_or_default();
        let (value, truncated) = bounded_string(raw);
        ((!value.is_empty()).then_some(value), truncated)
    };
    let (value, value_truncated) = if is_protected {
        (None, false)
    } else {
        let (value, truncated) = bounded_string(
            copy_attribute(element, "AXValue")
                .and_then(|value| native_value_text(value.0))
                .unwrap_or_default(),
        );
        ((!value.is_empty()).then_some(value), truncated)
    };
    let mut supported_actions = Vec::new();
    if !is_protected {
        let actions = action_names(element);
        if actions
            .iter()
            .any(|name| name == "AXPress" || name == "AXConfirm")
        {
            supported_actions.push(UiSemanticActionKind::Invoke);
        }
        if actions.iter().any(|name| name == "AXPress") && attribute_toggle_state(element).is_some()
        {
            supported_actions.push(UiSemanticActionKind::Toggle);
        }
        if attribute_settable(element, "AXSelected") {
            supported_actions.push(UiSemanticActionKind::Select);
        }
        if attribute_settable(element, "AXValue")
            && copy_attribute(element, "AXValue")
                .is_some_and(|value| native_value_supported(value.0))
        {
            supported_actions.push(UiSemanticActionKind::SetValue);
        }
        if attribute_settable(element, "AXFocused") {
            supported_actions.push(UiSemanticActionKind::Focus);
        }
    }

    (
        CollectedUiNode {
            is_collection: matches!(role.as_str(), "AXGrid" | "AXOutline" | "AXTable"),
            native_id: (!is_protected && !identifier.is_empty() && identifier.len() <= 512)
                .then(|| identifier.clone()),
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

pub(super) fn application_by_pid(process_id: u32) -> Result<ObservedApplication, AgentError> {
    autoreleasepool(|_| unsafe {
        let application: *mut AnyObject = msg_send![class!(NSRunningApplication), runningApplicationWithProcessIdentifier: process_id as i32];
        application_identity(application)
    })
}

pub(super) fn application_display_name(pid: u32) -> Option<String> {
    autoreleasepool(|_| unsafe {
        let app: *mut AnyObject = msg_send![class!(NSRunningApplication), runningApplicationWithProcessIdentifier: pid as i32];
        if app.is_null() {
            return None;
        }
        let name: *mut AnyObject = msg_send![app, localizedName];
        if name.is_null() {
            return None;
        }
        let bytes: *const c_char = msg_send![name, UTF8String];
        if bytes.is_null() {
            None
        } else {
            Some(
                std::ffi::CStr::from_ptr(bytes)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    })
}

/// Metadata only: do not traverse the AX trees of unselected applications.
pub(super) fn running_applications() -> Result<Vec<ObservedApplication>, AgentError> {
    autoreleasepool(|_| unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let applications: *mut AnyObject = msg_send![workspace, runningApplications];
        if applications.is_null() {
            return Ok(Vec::new());
        }
        let count: usize = msg_send![applications, count];
        let mut result = Vec::new();
        for index in 0..count.min(4096) {
            let application: *mut AnyObject = msg_send![applications, objectAtIndex: index];
            if application.is_null() {
                continue;
            }
            let activation_policy: isize = msg_send![application, activationPolicy];
            if activation_policy != 0 {
                continue;
            }
            if let Ok(identity) = application_identity(application) {
                result.push(identity);
            }
        }
        result.sort_by(|a, b| (&a.image_path, a.process_id).cmp(&(&b.image_path, b.process_id)));
        Ok(result)
    })
}

fn frontmost_application() -> Result<ObservedApplication, AgentError> {
    autoreleasepool(|_| unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let application: *mut AnyObject = msg_send![workspace, frontmostApplication];
        application_identity(application)
    })
}

unsafe fn application_identity(
    application: *mut AnyObject,
) -> Result<ObservedApplication, AgentError> {
    if application.is_null() {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the selected macOS application is no longer running",
            true,
        ));
    }
    // SAFETY: callers keep the NSRunningApplication alive inside an autorelease pool.
    unsafe {
        let terminated: bool = msg_send![application, isTerminated];
        let process_id: i32 = msg_send![application, processIdentifier];
        let executable_url: *mut AnyObject = msg_send![application, executableURL];
        if terminated || process_id <= 0 || executable_url.is_null() {
            return Err(failure(
                AgentErrorKind::SessionUnavailable,
                "cannot resolve the macOS application identity",
                true,
            ));
        }
        let path: *mut AnyObject = msg_send![executable_url, path];
        let utf8: *const c_char = msg_send![path, UTF8String];
        if utf8.is_null() {
            return Err(failure(
                AgentErrorKind::SessionUnavailable,
                "cannot resolve the macOS application path",
                true,
            ));
        }
        Ok(ObservedApplication {
            window_handle: 0,
            process_id: process_id as u32,
            image_path: CStr::from_ptr(utf8).to_string_lossy().into_owned(),
            process_started_at: Some(process_start(process_id as u32)?),
        })
    }
}

fn copy_attribute(element: AxUiElementRef, attribute: &str) -> Option<OwnedCf> {
    let attribute = create_string(attribute)?;
    let mut value = std::ptr::null();
    let status = unsafe { AXUIElementCopyAttributeValue(element, attribute.0, &mut value) };
    (status == AX_SUCCESS && !value.is_null()).then_some(OwnedCf(value))
}

fn attribute_string(element: AxUiElementRef, attribute: &str) -> Option<String> {
    let value = copy_attribute(element, attribute)?;
    cf_string(value.0)
}

const CF_DATE_UNIX_OFFSET: f64 = 978_307_200.0;
const CF_NUMBER_DOUBLE_TYPE: i32 = 13;

fn native_value_supported(value: CfTypeRef) -> bool {
    let kind = unsafe { CFGetTypeID(value) };
    kind == unsafe { CFStringGetTypeID() }
        || kind == unsafe { CFDateGetTypeID() }
        || kind == unsafe { CFNumberGetTypeID() }
}

fn native_date(value: CfTypeRef) -> Option<chrono::DateTime<chrono::Local>> {
    let seconds = unsafe { CFDateGetAbsoluteTime(value) } + CF_DATE_UNIX_OFFSET;
    if !seconds.is_finite() {
        return None;
    }
    chrono::DateTime::from_timestamp_millis((seconds * 1000.0) as i64)
        .map(|at| at.with_timezone(&chrono::Local))
}

fn native_value_text(value: CfTypeRef) -> Option<String> {
    let kind = unsafe { CFGetTypeID(value) };
    if kind == unsafe { CFDateGetTypeID() } {
        return native_date(value).map(|at| at.to_rfc3339());
    }
    if kind == unsafe { CFNumberGetTypeID() } {
        let mut number = 0.0_f64;
        return (unsafe {
            CFNumberGetValue(
                value,
                CF_NUMBER_DOUBLE_TYPE,
                std::ptr::addr_of_mut!(number).cast(),
            )
        } && number.is_finite())
        .then(|| number.to_string());
    }
    cf_string(value)
}

fn parse_native_date(
    value: &str,
    current: chrono::DateTime<chrono::Local>,
) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::TimeZone;
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(at.with_timezone(&chrono::Local));
    }
    let local = if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        date.and_time(current.time())
    } else {
        let time = chrono::NaiveTime::parse_from_str(value, "%H:%M:%S")
            .or_else(|_| chrono::NaiveTime::parse_from_str(value, "%H:%M"))
            .ok()?;
        current.date_naive().and_time(time)
    };
    // Ambiguous/nonexistent local times require an explicit RFC3339 offset.
    chrono::Local.from_local_datetime(&local).single()
}

fn encode_native_value(current: CfTypeRef, value: &str) -> Result<OwnedCf, AgentError> {
    let invalid = || {
        failure(
            AgentErrorKind::InvalidInput,
            "Invalid native AXValue. Strings require text; numbers require a finite decimal; dates require RFC3339 with offset, YYYY-MM-DD (preserves local time), or HH:MM[:SS] (preserves local date). No write was attempted. Read the current UI value before correcting the input.",
            false,
        )
    };
    let kind = unsafe { CFGetTypeID(current) };
    if kind == unsafe { CFStringGetTypeID() } {
        return create_string(value).ok_or_else(invalid);
    }
    let native = if kind == unsafe { CFDateGetTypeID() } {
        let at = parse_native_date(value, native_date(current).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        unsafe {
            CFDateCreate(
                std::ptr::null(),
                at.timestamp_millis() as f64 / 1000.0 - CF_DATE_UNIX_OFFSET,
            )
        }
    } else if kind == unsafe { CFNumberGetTypeID() } {
        let number: f64 = value.parse().map_err(|_| invalid())?;
        if !number.is_finite() {
            return Err(invalid());
        }
        unsafe {
            CFNumberCreate(
                std::ptr::null(),
                CF_NUMBER_DOUBLE_TYPE,
                std::ptr::addr_of!(number).cast(),
            )
        }
    } else {
        return Err(invalid());
    };
    if native.is_null() {
        return Err(invalid());
    }
    Ok(OwnedCf(native))
}

fn attribute_bool(element: AxUiElementRef, attribute: &str) -> Option<bool> {
    let value = copy_attribute(element, attribute)?;
    if unsafe { CFGetTypeID(value.0) } != unsafe { CFBooleanGetTypeID() } {
        return None;
    }
    Some(unsafe { CFBooleanGetValue(value.0) })
}

fn attribute_point(element: AxUiElementRef, attribute: &str) -> Option<CGPoint> {
    let value = copy_attribute(element, attribute)?;
    if unsafe { AXValueGetType(value.0) } != AX_VALUE_CGPOINT_TYPE {
        return None;
    }
    let mut point = CGPoint::default();
    unsafe {
        AXValueGetValue(
            value.0,
            AX_VALUE_CGPOINT_TYPE,
            (&mut point as *mut CGPoint).cast(),
        )
    }
    .then_some(point)
}

fn attribute_size(element: AxUiElementRef, attribute: &str) -> Option<CGSize> {
    let value = copy_attribute(element, attribute)?;
    if unsafe { AXValueGetType(value.0) } != AX_VALUE_CGSIZE_TYPE {
        return None;
    }
    let mut size = CGSize::default();
    unsafe {
        AXValueGetValue(
            value.0,
            AX_VALUE_CGSIZE_TYPE,
            (&mut size as *mut CGSize).cast(),
        )
    }
    .then_some(size)
}

fn attribute_toggle_state(element: AxUiElementRef) -> Option<bool> {
    let value = copy_attribute(element, "AXValue")?;
    let type_id = unsafe { CFGetTypeID(value.0) };
    if type_id == unsafe { CFBooleanGetTypeID() } {
        return Some(unsafe { CFBooleanGetValue(value.0) });
    }
    if type_id != unsafe { CFNumberGetTypeID() } {
        return None;
    }
    let mut number = 0_i64;
    if unsafe {
        CFNumberGetValue(
            value.0,
            CF_NUMBER_SINT64_TYPE,
            std::ptr::addr_of_mut!(number).cast(),
        )
    } {
        toggle_state_from_number(number)
    } else {
        None
    }
}

fn toggle_state_from_number(number: i64) -> Option<bool> {
    match number {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn attribute_settable(element: AxUiElementRef, attribute: &str) -> bool {
    let Some(attribute) = create_string(attribute) else {
        return false;
    };
    let mut settable = false;
    (unsafe { AXUIElementIsAttributeSettable(element, attribute.0, &mut settable) } == AX_SUCCESS)
        && settable
}

fn perform_action(element: AxUiElementRef, action: &str) -> Result<(), AgentError> {
    let action = create_string(action).ok_or_else(|| {
        failure(
            AgentErrorKind::Internal,
            "cannot encode the Accessibility action name",
            false,
        )
    })?;
    mutation_with_timeout(element, "AXUIElementPerformAction", || unsafe {
        AXUIElementPerformAction(element, action.0)
    })
}

fn mutation_with_timeout(
    element: AxUiElementRef,
    operation: &str,
    mutate: impl FnOnce() -> i32,
) -> Result<(), AgentError> {
    if unsafe { AXUIElementSetMessagingTimeout(element, AX_ACTION_TIMEOUT_SECONDS) } != AX_SUCCESS {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "cannot configure Accessibility action timeout; action was not invoked",
            false,
        ));
    }
    let started = Instant::now();
    let status = mutate();
    let elapsed_ms = started.elapsed().as_millis();
    log::info!(
        "[accessibility-action] operation={operation} status={status} elapsed_ms={elapsed_ms} timeout_seconds={AX_ACTION_TIMEOUT_SECONDS}"
    );
    accessibility_mutation_status(
        status,
        &format!(
            "{operation} (elapsed_ms={elapsed_ms}, timeout_seconds={AX_ACTION_TIMEOUT_SECONDS})"
        ),
    )
}

// A nonzero native return is not proof that a mutation had no effect.
fn accessibility_mutation_status(status: i32, operation: &str) -> Result<(), AgentError> {
    if status == AX_SUCCESS {
        return Ok(());
    }
    let label = match status {
        -25200 => "failure",
        -25201 => "illegal_argument",
        -25202 => "invalid_ui_element",
        -25204 => "cannot_complete",
        -25206 => "action_unsupported",
        -25211 => "api_disabled",
        _ => "native_error",
    };
    Err(failure(
        AgentErrorKind::Internal,
        &format!(
            "{operation} returned AXError {status} ({label}); the operation may have taken effect. Do not retry automatically; inspect the current UI to verify the result."
        ),
        false,
    ))
}

fn set_messaging_timeout(element: AxUiElementRef) -> Result<(), AgentError> {
    if unsafe { AXUIElementSetMessagingTimeout(element, AX_MESSAGE_TIMEOUT_SECONDS) } == AX_SUCCESS
    {
        Ok(())
    } else {
        Err(failure(
            AgentErrorKind::SessionUnavailable,
            "cannot bound Accessibility messaging for the target application",
            true,
        ))
    }
}

fn set_bool_attribute(
    element: AxUiElementRef,
    attribute: &str,
    value: bool,
) -> Result<(), AgentError> {
    let value = unsafe {
        if value {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        }
    };
    set_attribute(element, attribute, value)
}

fn set_attribute(
    element: AxUiElementRef,
    attribute: &str,
    value: CfTypeRef,
) -> Result<(), AgentError> {
    let attribute = create_string(attribute).ok_or_else(|| {
        failure(
            AgentErrorKind::Internal,
            "cannot encode the Accessibility attribute name",
            false,
        )
    })?;
    mutation_with_timeout(element, "AXUIElementSetAttributeValue", || unsafe {
        AXUIElementSetAttributeValue(element, attribute.0, value)
    })
}

fn action_names(element: AxUiElementRef) -> Vec<String> {
    let mut names = std::ptr::null();
    if unsafe { AXUIElementCopyActionNames(element, &mut names) } != AX_SUCCESS || names.is_null() {
        return Vec::new();
    }
    let names = OwnedCf(names);
    let count = unsafe { CFArrayGetCount(names.0) }.max(0);
    (0..count)
        .filter_map(|index| cf_string(unsafe { CFArrayGetValueAtIndex(names.0, index) }))
        .collect()
}

fn create_string(value: &str) -> Option<OwnedCf> {
    let value = CString::new(value).ok()?;
    let string = unsafe {
        CFStringCreateWithCString(std::ptr::null(), value.as_ptr(), CF_STRING_ENCODING_UTF8)
    };
    (!string.is_null()).then_some(OwnedCf(string))
}

fn cf_string(value: CfTypeRef) -> Option<String> {
    if value.is_null() || unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
        return None;
    }
    let length = unsafe { CFStringGetLength(value) };
    let capacity = unsafe { CFStringGetMaximumSizeForEncoding(length, CF_STRING_ENCODING_UTF8) }
        .checked_add(1)?;
    let mut buffer = vec![0_u8; capacity as usize];
    if !unsafe {
        CFStringGetCString(
            value,
            buffer.as_mut_ptr().cast(),
            capacity,
            CF_STRING_ENCODING_UTF8,
        )
    } {
        return None;
    }
    CStr::from_bytes_until_nul(&buffer)
        .ok()
        .map(|value| value.to_string_lossy().into_owned())
}

fn process_start(process_id: u32) -> Result<u64, AgentError> {
    libproc::pid_rusage::pidrusage::<libproc::pid_rusage::RUsageInfoV2>(process_id as i32)
        .map(|usage| usage.ri_proc_start_abstime)
        .map_err(|_| {
            failure(
                AgentErrorKind::SessionUnavailable,
                "cannot bind the foreground application to its process incarnation",
                true,
            )
        })
        .and_then(|started_at| {
            (started_at != 0).then_some(started_at).ok_or_else(|| {
                failure(
                    AgentErrorKind::SessionUnavailable,
                    "cannot bind the foreground application to its process incarnation",
                    true,
                )
            })
        })
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

thread_local! { static IDENTITIES: RefCell<IdentityStore<OwnedCf>> = RefCell::new(IdentityStore::new(8192)); }

impl NativeElement for OwnedCf {
    fn same_element(&self, other: &Self) -> bool {
        unsafe { CFEqual(self.0, other.0) != 0 }
    }
    fn definitely_destroyed(&self) -> bool {
        let Some(attribute) = create_string("AXRole") else {
            return false;
        };
        let mut value = std::ptr::null();
        let status = unsafe { AXUIElementCopyAttributeValue(self.0, attribute.0, &mut value) };
        if !value.is_null() {
            drop(OwnedCf(value));
        }
        // kAXErrorInvalidUIElement; permission/timeout errors preserve identity.
        status == -25202
    }
}

fn failure(kind: AgentErrorKind, message: &str, retryable: bool) -> AgentError {
    AgentError {
        kind,
        message: message.into(),
        retryable,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_mutation_errors_preserve_code_without_claiming_no_effect() {
        assert!(accessibility_mutation_status(0, "action").is_ok());
        for status in [-25202, -25204, -25206, -999] {
            let error = accessibility_mutation_status(status, "action").unwrap_err();
            assert!(error.message.contains(&status.to_string()));
            assert!(error.message.contains("may have taken effect"));
            assert!(!error.retryable);
        }
    }

    #[test]
    #[ignore = "requires background Calculator, Accessibility and Screen Recording permissions"]
    fn live_calculator_precise_read_and_independent_capture() {
        use desk_agent_protocol::computer_use::UiInspectQuery;
        let app = running_applications()
            .unwrap()
            .into_iter()
            .find(|app| app.image_path.ends_with("/Calculator"))
            .expect("running Calculator");
        assert_ne!(
            frontmost_application().unwrap().process_id,
            app.process_id,
            "Calculator must remain in background"
        );
        let all = collect_application(app.process_id, &app.image_path, 12, 300, 262144).unwrap();
        let query = UiInspectQuery {
            element_id: None,
            any: Vec::new(),
            role: Some("AXStaticText".into()),
            ..Default::default()
        };
        let found = collect_application_selection(
            app.process_id,
            &app.image_path,
            12,
            300,
            262144,
            UiInspectScope::Content,
            None,
            Some(&query),
            false,
        )
        .unwrap();
        assert!(!found.truncated);
        assert!(!found.nodes.is_empty());
        assert!(found.nodes.iter().all(|node| node.role == "AXStaticText"));
        let display = found
            .nodes
            .iter()
            .find(|node| node.value.is_some())
            .unwrap();
        let single = collect_application_selection(
            app.process_id,
            &app.image_path,
            12,
            300,
            262144,
            UiInspectScope::Content,
            Some(&display.fingerprint),
            None,
            true,
        )
        .unwrap();
        assert_eq!(single.nodes.len(), 1);
        assert_eq!(single.nodes[0].value, display.value);
        assert_eq!(single.nodes[0].fingerprint, display.fingerprint);
        let identified = all
            .nodes
            .iter()
            .find(|node| node.native_id.is_some())
            .expect("a native identifier");
        let query = UiInspectQuery {
            element_id: None,
            any: Vec::new(),
            native_id: identified.native_id.clone(),
            ..Default::default()
        };
        let by_id = collect_application_selection(
            app.process_id,
            &app.image_path,
            12,
            300,
            262144,
            UiInspectScope::Content,
            None,
            Some(&query),
            false,
        )
        .unwrap();
        assert!(
            by_id
                .nodes
                .iter()
                .any(|node| node.fingerprint == identified.fingerprint)
        );
        let query = UiInspectQuery {
            element_id: None,
            any: Vec::new(),
            name: Some("nonexistent-precise-query".into()),
            ..Default::default()
        };
        assert!(
            collect_application_selection(
                app.process_id,
                &app.image_path,
                12,
                300,
                262144,
                UiInspectScope::Content,
                None,
                Some(&query),
                false
            )
            .unwrap()
            .nodes
            .is_empty()
        );
        assert!(
            collect_application_selection(
                app.process_id,
                &app.image_path,
                12,
                300,
                262144,
                UiInspectScope::Content,
                Some("stale-fingerprint"),
                None,
                true
            )
            .is_err()
        );
        let window = all
            .nodes
            .iter()
            .find(|node| node.role.starts_with("AXWindow"))
            .unwrap();
        let target =
            resolve_window_capture_target(app.process_id, &app.image_path, &window.fingerprint)
                .unwrap();
        let settings = desk_signal_facade::model::desk_settings::DeskSettings {
            video_device_name: core_graphics::display::CGDisplay::main().id.to_string(),
            ..Default::default()
        };
        let shot = super::super::collectors::screen_capture::collect(
            &desk_agent_protocol::ScreenCaptureParams::default(),
            &settings,
            Some(target),
        )
        .unwrap();
        assert!(shot.width > 0 && shot.height > 0);
        assert_eq!(&shot.image[..4], &[0x89, b'P', b'N', b'G']);
        if let Ok(path) = std::env::var("LRDM_WINDOW_CAPTURE_TEST_OUTPUT") {
            std::fs::write(path, &shot.image).unwrap();
        }
        assert_ne!(
            frontmost_application().unwrap().process_id,
            app.process_id,
            "capture must not activate Calculator"
        );
        println!(
            "Precise UI: all={}, text={}, element={}; background PNG={}x{}",
            all.nodes.len(),
            found.nodes.len(),
            single.nodes.len(),
            shot.width,
            shot.height
        );
    }

    #[test]
    fn menu_classification_does_not_hide_ordinary_controls() {
        for role in ["AXMenu", "AXMenuBar", "AXMenuItem", "AXMenuBarItem"] {
            assert!(is_menu_role(role));
        }
        for role in [
            "AXButton",
            "AXStaticText",
            "AXWindow",
            "AXGroup",
            "AXPopUpButton",
        ] {
            assert!(!is_menu_role(role));
        }
    }

    #[test]
    #[ignore = "requires a running Calculator and Accessibility permission"]
    fn live_calculator_content_and_menu_scopes_preserve_identity() {
        let calculator = running_applications()
            .unwrap()
            .into_iter()
            .find(|app| app.image_path.ends_with("/Calculator"))
            .expect("running Calculator");
        let read = |scope| {
            collect_application_with_scope(
                calculator.process_id,
                &calculator.image_path,
                12,
                1024,
                1024 * 1024,
                scope,
            )
            .unwrap()
        };
        let all = read(UiInspectScope::All);
        let content = read(UiInspectScope::Content);
        let menus = read(UiInspectScope::Menus);
        assert!(!all.truncated && !content.truncated && !menus.truncated);
        assert!(!content.nodes.is_empty() && !menus.nodes.is_empty());
        assert!(content.nodes.iter().all(|node| !is_menu_role(&node.role)));
        assert!(menus.nodes.iter().all(|node| is_menu_role(&node.role)));
        assert_eq!(all.nodes.len(), content.nodes.len() + menus.nodes.len());
        assert!(
            content
                .nodes
                .iter()
                .any(|node| node.role == "AXStaticText" && node.value.is_some())
        );
        for node in content.nodes.iter().chain(&menus.nodes) {
            let original =
                all.nodes
                    .iter()
                    .find(|original| original.fingerprint == node.fingerprint)
                    .unwrap_or_else(|| {
                        panic!(
                            "identity mismatch: {node:?}; same name: {:?}",
                            all.nodes
                                .iter()
                                .filter(|original| original.name == node.name
                                    && original.role == node.role)
                                .collect::<Vec<_>>()
                        )
                    });
            assert_eq!(original.name, node.name);
            assert_eq!(original.value, node.value);
        }
        let menu = menus
            .nodes
            .iter()
            .find(|node| {
                node.supported_actions
                    .contains(&UiSemanticActionKind::Invoke)
            })
            .expect("actionable menu");
        preflight_action(
            calculator.process_id,
            &calculator.image_path,
            &menu.fingerprint,
            &UiSemanticAction::Invoke,
        )
        .unwrap();
        let window = all
            .nodes
            .iter()
            .find(|node| node.role.starts_with("AXWindow"))
            .unwrap();
        let window_menus = collect_application_with_window_scope(
            calculator.process_id,
            &calculator.image_path,
            12,
            1024,
            1024 * 1024,
            UiInspectScope::Menus,
            Some(&window.fingerprint),
        )
        .unwrap();
        assert!(
            window_menus.nodes.is_empty(),
            "Calculator menu bar belongs to the application, not its window"
        );
        println!(
            "Calculator scopes: all={}, content={}, menus={}",
            all.nodes.len(),
            content.nodes.len(),
            menus.nodes.len()
        );
    }

    #[test]
    #[ignore = "requires a running background Calculator and Accessibility permission"]
    fn live_background_calculator_can_be_selected_without_activation() {
        let foreground = frontmost_application().unwrap();
        let calculator = running_applications()
            .unwrap()
            .into_iter()
            .find(|application| application.image_path.ends_with("/Calculator"))
            .expect("running Calculator");
        assert_ne!(
            foreground.process_id, calculator.process_id,
            "Calculator must be in the background"
        );
        let tree = collect_application(
            calculator.process_id,
            &calculator.image_path,
            16,
            1024,
            1024 * 1024,
        )
        .unwrap();
        assert!(!tree.nodes.is_empty());
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.role.starts_with("AXWindow"))
        );
        assert_eq!(
            frontmost_application().unwrap().process_id,
            foreground.process_id
        );
        assert_eq!(
            application_by_pid(calculator.process_id)
                .unwrap()
                .process_started_at,
            calculator.process_started_at
        );
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
    fn toggle_state_rejects_mixed_or_unknown_numeric_values() {
        assert_eq!(toggle_state_from_number(0), Some(false));
        assert_eq!(toggle_state_from_number(1), Some(true));
        assert_eq!(toggle_state_from_number(2), None);
        assert_eq!(toggle_state_from_number(-1), None);
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
    #[ignore = "requires a macOS Aqua session with Accessibility permission"]
    fn live_session_probe_finds_the_frontmost_application() {
        let observed = observe_interactive_desktop().expect("interactive macOS session");
        let application = observed
            .foreground_application
            .expect("frontmost macOS application");
        assert!(application.process_id > 0);
        assert!(application.image_path.starts_with('/'));
    }

    #[test]
    #[ignore = "requires Calculator to be frontmost with Accessibility permission"]
    fn live_calculator_tree_and_invoke_use_the_production_ax_adapter() {
        let observed = observe_interactive_desktop().expect("interactive macOS session");
        let application = observed
            .foreground_application
            .expect("frontmost Calculator application");
        assert!(
            application.image_path.contains("/Calculator.app/"),
            "Calculator must be frontmost, got {}",
            application.image_path
        );
        let tree = collect_foreground(
            application.process_id,
            &application.image_path,
            8,
            512,
            256 * 1024,
        )
        .expect("bounded Calculator Accessibility tree");
        let button = tree
            .nodes
            .iter()
            .find(|node| {
                node.name.as_deref() == Some("1")
                    && node
                        .supported_actions
                        .contains(&UiSemanticActionKind::Invoke)
            })
            .expect("Calculator digit 1 button");
        preflight_action(
            application.process_id,
            &application.image_path,
            &button.fingerprint,
            &UiSemanticAction::Invoke,
        )
        .expect("Calculator invoke preflight");
        let result = apply_action(
            application.process_id,
            &application.image_path,
            &button.fingerprint,
            &UiSemanticAction::Invoke,
        )
        .expect("Calculator invoke");
        assert!(result.changed);
        assert!(result.verified);
    }

    #[test]
    #[ignore = "requires Calculator to be frontmost, an Accessibility grant, and an external Calculator restart after the ready marker is written"]
    fn live_calculator_restart_rejects_the_stale_element_reference() {
        let observed = observe_interactive_desktop().expect("interactive macOS session");
        let application = observed
            .foreground_application
            .expect("frontmost Calculator application");
        assert!(
            application.image_path.contains("/Calculator.app/"),
            "Calculator must be frontmost, got {}",
            application.image_path
        );
        let tree = collect_foreground(
            application.process_id,
            &application.image_path,
            8,
            512,
            256 * 1024,
        )
        .expect("bounded Calculator Accessibility tree");
        let original_started_at =
            process_start(application.process_id).expect("Calculator process incarnation");
        let button = tree
            .nodes
            .iter()
            .find(|node| {
                node.name.as_deref() == Some("1")
                    && node
                        .supported_actions
                        .contains(&UiSemanticActionKind::Invoke)
            })
            .expect("Calculator digit 1 button");
        let stale_fingerprint = button.fingerprint.clone();

        let marker = std::env::var_os("LRD_AX_RESTART_READY_FILE")
            .expect("LRD_AX_RESTART_READY_FILE must name an external-harness marker");
        std::fs::write(&marker, application.process_id.to_string())
            .expect("write Calculator restart ready marker");

        let deadline = Instant::now() + Duration::from_secs(30);
        let (restarted_process_id, restarted_at) = 'restart: loop {
            assert!(
                Instant::now() < deadline,
                "Calculator was not restarted in time"
            );
            std::thread::sleep(Duration::from_millis(100));
            let process_ids = libproc::processes::pids_by_type(libproc::processes::ProcFilter::All)
                .expect("enumerate processes after Calculator restart");
            for process_id in process_ids {
                if process_id == 0 || process_id == application.process_id {
                    continue;
                }
                let Ok(image_path) = libproc::proc_pid::pidpath(process_id as i32) else {
                    continue;
                };
                if image_path == application.image_path
                    && let Ok(started_at) = process_start(process_id)
                {
                    break 'restart (process_id, started_at);
                }
            }
        };
        assert_ne!(restarted_at, original_started_at);

        let stale_for_lookup = stale_fingerprint.clone();
        assert!(
            super::super::native_ui_identity::run(move || Ok(retained_element(
                &stale_for_lookup,
                restarted_process_id,
                restarted_at
            )
            .is_err()))
            .unwrap(),
            "an old native element must not resolve in a restarted process"
        );

        let error = preflight_action(
            application.process_id,
            &application.image_path,
            &stale_fingerprint,
            &UiSemanticAction::Invoke,
        )
        .expect_err("an action bound to the old Calculator process must fail closed");
        assert_eq!(error.kind, AgentErrorKind::SessionUnavailable);
        let _ = std::fs::remove_file(marker);
    }
}

pub(super) fn retained_element_ids() -> Result<std::collections::HashSet<String>, AgentError> {
    super::native_ui_identity::run(|| Ok(IDENTITIES.with(|store| store.borrow().retained_ids())))
}

#[cfg(test)]
mod native_identity_tests {
    use super::*;
    #[test]
    fn repeated_native_application_handles_compare_as_one_identity() {
        super::super::native_ui_identity::run(|| {
            let pid = std::process::id();
            let first = OwnedCf(unsafe { AXUIElementCreateApplication(pid as i32) });
            let second = OwnedCf(unsafe { AXUIElementCreateApplication(pid as i32) });
            assert!(!first.0.is_null() && !second.0.is_null());
            assert!(first.same_element(&second));
            let first_key = unsafe { CFHash(first.0) }.to_le_bytes().to_vec();
            let second_key = unsafe { CFHash(second.0) }.to_le_bytes().to_vec();
            let mut registry = IdentityStore::new(4);
            let id = registry.identify(pid, 1, first_key, first)?;
            assert_eq!(registry.identify(pid, 1, second_key, second)?, id);
            Ok(())
        })
        .unwrap();
    }
}

fn retained_element(id: &str, process_id: u32, started: u64) -> Result<OwnedCf, AgentError> {
    IDENTITIES.with(|store| store.borrow_mut().get(id, process_id, started))
        .ok_or_else(|| failure(AgentErrorKind::InvalidInput, "the native UI element was destroyed or belongs to a different process lifetime; search for a new element", false))
}

#[cfg(test)]
mod native_value_tests {
    use super::*;

    #[test]
    fn dates_are_readable_and_written_as_dates() {
        let native = OwnedCf(unsafe { CFDateCreate(std::ptr::null(), 810_000_000.0) });
        let before = native_date(native.0).unwrap();
        assert!(native_value_text(native.0).unwrap().contains('T'));
        let changed = encode_native_value(native.0, "2026-09-11").unwrap();
        assert_eq!(unsafe { CFGetTypeID(changed.0) }, unsafe {
            CFDateGetTypeID()
        });
        let after = native_date(changed.0).unwrap();
        assert_eq!(after.date_naive().to_string(), "2026-09-11");
        assert_eq!(after.time(), before.time());
        let time = encode_native_value(changed.0, "09:00").unwrap();
        assert_eq!(
            native_date(time.0)
                .unwrap()
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            "2026-09-11 09:00"
        );
        assert!(encode_native_value(native.0, "9/11/2026").is_err());
    }

    #[test]
    fn numeric_and_string_values_keep_their_native_types() {
        let text = create_string("old").unwrap();
        assert_eq!(
            native_value_text(encode_native_value(text.0, "new").unwrap().0).as_deref(),
            Some("new")
        );
        let number = 12.5_f64;
        let native = OwnedCf(unsafe {
            CFNumberCreate(
                std::ptr::null(),
                CF_NUMBER_DOUBLE_TYPE,
                std::ptr::addr_of!(number).cast(),
            )
        });
        let changed = encode_native_value(native.0, "42.25").unwrap();
        assert_eq!(native_value_text(changed.0).as_deref(), Some("42.25"));
        assert!(encode_native_value(native.0, "NaN").is_err());
        assert!(!native_value_supported(unsafe { kCFBooleanTrue }));
    }
}

/// Resolve the exact native window and optional control without activating the application.
pub(super) fn background_target(
    pid: u32,
    image_path: String,
    window_fingerprint: String,
    element_fingerprint: Option<String>,
    keyboard: bool,
) -> Result<super::macos_background_input::Target, AgentError> {
    super::native_ui_identity::run(move || {
        let window = locate_action_target(pid, &image_path, &window_fingerprint)?;
        let bad = || {
            failure(
                AgentErrorKind::SessionUnavailable,
                "Background input window is unavailable; read the application's windows again",
                false,
            )
        };
        if attribute_string(window.0, "AXRole").as_deref() != Some("AXWindow")
            || attribute_bool(window.0, "AXMinimized").unwrap_or(false)
        {
            return Err(bad());
        }
        let app = OwnedCf(unsafe { AXUIElementCreateApplication(pid as i32) });
        if keyboard {
            let focused = copy_attribute(app.0, "AXFocusedWindow").ok_or_else(bad)?;
            if !focused.same_element(&window) {
                return Err(failure(
                    AgentErrorKind::SessionUnavailable,
                    "The requested window is not this application's keyboard input window; inspect its windows before typing. No input was dispatched",
                    false,
                ));
            }
        }
        let origin = attribute_point(window.0, "AXPosition").ok_or_else(bad)?;
        let size = attribute_size(window.0, "AXSize").ok_or_else(bad)?;
        if ![origin.x, origin.y, size.width, size.height]
            .iter()
            .all(|v| v.is_finite())
            || size.width <= 0.0
            || size.height <= 0.0
        {
            return Err(bad());
        }
        // Keyboard delivery needs only the application's input window, not
        // screen-recording access or a CoreGraphics window number.
        if keyboard {
            return Ok(super::macos_background_input::Target {
                pid,
                window_id: 0,
                origin,
                width: size.width,
                height: size.height,
                element_point: None,
            });
        }
        let point = if let Some(fingerprint) = element_fingerprint {
            let element = locate_action_target(pid, &image_path, &fingerprint)?;
            let owner = copy_attribute(element.0, "AXWindow").ok_or_else(bad)?;
            if !owner.same_element(&window) {
                return Err(failure(
                    AgentErrorKind::InvalidInput,
                    "Mouse element belongs to another window; no input was dispatched",
                    false,
                ));
            }
            let p = attribute_point(element.0, "AXPosition").ok_or_else(bad)?;
            let s = attribute_size(element.0, "AXSize").ok_or_else(bad)?;
            let mut left = p.x.max(origin.x);
            let mut top = p.y.max(origin.y);
            let mut right = (p.x + s.width).min(origin.x + size.width);
            let mut bottom = (p.y + s.height).min(origin.y + size.height);
            let mut parent = copy_attribute(element.0, "AXParent");
            for _ in 0..32 {
                let Some(current) = parent else { break };
                if current.same_element(&window) {
                    break;
                }
                if attribute_string(current.0, "AXRole").as_deref() == Some("AXScrollArea") {
                    if let (Some(p), Some(s)) = (
                        attribute_point(current.0, "AXPosition"),
                        attribute_size(current.0, "AXSize"),
                    ) {
                        left = left.max(p.x);
                        top = top.max(p.y);
                        right = right.min(p.x + s.width);
                        bottom = bottom.min(p.y + s.height);
                    }
                }
                parent = copy_attribute(current.0, "AXParent");
            }
            if left >= right || top >= bottom {
                return Err(failure(
                    AgentErrorKind::InvalidInput,
                    "Mouse element is outside the visible window; read or scroll before clicking",
                    false,
                ));
            }
            Some(CGPoint::new((left + right) / 2.0, (top + bottom) / 2.0))
        } else {
            None
        };
        let title = attribute_string(window.0, "AXTitle").unwrap_or_default();
        let list = OwnedCf(unsafe { CGWindowListCopyWindowInfo(0, 0) });
        if list.0.is_null() {
            return Err(bad());
        }
        let mut candidates = Vec::new();
        for i in 0..unsafe { CFArrayGetCount(list.0) } {
            let row = unsafe { CFArrayGetValueAtIndex(list.0, i) };
            let get = |key: &str| -> CfTypeRef {
                let Some(k) = create_string(key) else {
                    return std::ptr::null();
                };
                unsafe { CFDictionaryGetValue(row, k.0) }
            };
            let number = |key: &str| -> Option<i64> {
                let v = get(key);
                if v.is_null() {
                    return None;
                }
                let mut n = 0i64;
                unsafe { CFNumberGetValue(v, 4, (&mut n as *mut i64).cast()) }.then_some(n)
            };
            let bounds = get("kCGWindowBounds");
            let mut rect = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(0.0, 0.0));
            let same_bounds = !bounds.is_null()
                && unsafe { CGRectMakeWithDictionaryRepresentation(bounds, &mut rect) }
                && [
                    rect.origin.x - origin.x,
                    rect.origin.y - origin.y,
                    rect.size.width - size.width,
                    rect.size.height - size.height,
                ]
                .iter()
                .all(|d| d.abs() < 1.0);
            // Window names may be redacted without capture permission. PID,
            // layer and current geometry must still identify exactly one window.
            let name = cf_string(get("kCGWindowName"));
            if number("kCGWindowOwnerPID") == Some(i64::from(pid))
                && number("kCGWindowLayer") == Some(0)
                && same_bounds
                && name
                    .as_ref()
                    .is_none_or(|name| name.is_empty() || name == &title)
            {
                if let Some(id) = number("kCGWindowNumber") {
                    candidates.push(id as u32);
                }
            }
        }
        if candidates.len() != 1 {
            return Err(failure(
                AgentErrorKind::SessionUnavailable,
                "Cannot uniquely map the observed window to its native window ID; no input was dispatched",
                false,
            ));
        }
        Ok(super::macos_background_input::Target {
            pid,
            window_id: candidates[0],
            origin,
            width: size.width,
            height: size.height,
            element_point: point,
        })
    })
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRectMakeWithDictionaryRepresentation(dictionary: CfTypeRef, rect: *mut CGRect) -> bool;
    fn CGWindowListCopyWindowInfo(options: u32, relative: u32) -> CfTypeRef;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDictionaryGetValue(dictionary: CfTypeRef, key: CfTypeRef) -> CfTypeRef;
}

#[cfg(test)]
#[path = "macos_background_input_live_tests.rs"]
mod background_input_live_tests;
