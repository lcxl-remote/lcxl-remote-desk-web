//! Bounded macOS Accessibility observation for the active Aqua session.

use desk_agent_protocol::computer_use::UiInspectScope;
use std::ffi::{CStr, CString, c_char, c_void};
use std::time::{Duration, Instant};

use core_graphics::geometry::{CGPoint, CGSize};
use desk_agent_protocol::computer_use::{UiSemanticAction, UiSemanticActionKind};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use sha2::{Digest, Sha256};

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
    fn CFRetain(value: CfTypeRef) -> CfTypeRef;
    fn CFGetTypeID(value: CfTypeRef) -> CfTypeId;
    fn CFStringGetTypeID() -> CfTypeId;
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
    );
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
    let element =
        locate_action_target(expected_process_id, expected_image_path, target_fingerprint)?;
    validate_action_target(element.0, action)
}

pub(super) fn resolve_window_capture_target(
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
            let value_ref = create_string(value).ok_or_else(|| {
                failure(
                    AgentErrorKind::InvalidInput,
                    "the Accessibility value contains an invalid NUL byte",
                    false,
                )
            })?;
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
    let root = unsafe { AXUIElementCreateApplication(expected_process_id as libc::pid_t) };
    if root.is_null() {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the selected application has no Accessibility root",
            false,
        ));
    }
    let root = OwnedCf(root);
    set_messaging_timeout(root.0)?;
    let config = WalkConfig {
        query: None,
        element_only: false,
        menu_window: None,
        scope: UiInspectScope::All,
        process_id: expected_process_id,
        process_started_at: process_start(expected_process_id)?,
        max_depth: 16,
        max_nodes: 1_024,
        max_bytes: usize::MAX,
        deadline: Instant::now() + HARD_DEADLINE,
    };
    let mut visited = 0usize;
    find_element(
        root.0,
        None,
        0,
        0,
        target_fingerprint,
        &config,
        &mut visited,
    )
    .ok_or_else(|| {
        failure(
            AgentErrorKind::InvalidInput,
            "the Accessibility element reference is stale or no longer reachable",
            false,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn find_element(
    element: AxUiElementRef,
    parent_fingerprint: Option<&str>,
    depth: u16,
    sibling_ordinal: usize,
    target_fingerprint: &str,
    config: &WalkConfig,
    visited: &mut usize,
) -> Option<OwnedCf> {
    if *visited >= config.max_nodes || Instant::now() >= config.deadline {
        return None;
    }
    if unsafe { AXUIElementSetMessagingTimeout(element, AX_MESSAGE_TIMEOUT_SECONDS) } != AX_SUCCESS
    {
        return None;
    }
    *visited += 1;
    let role = attribute_string(element, "AXRole").unwrap_or_else(|| "AXUnknown".into());
    let subrole = attribute_string(element, "AXSubrole").unwrap_or_default();
    let role = if subrole.is_empty() {
        role
    } else {
        format!("{role}/{subrole}")
    };
    let identifier = attribute_string(element, "AXIdentifier").unwrap_or_default();
    let current_fingerprint = fingerprint(
        parent_fingerprint,
        sibling_ordinal,
        config.process_id,
        config.process_started_at,
        &role,
        &identifier,
    );
    if current_fingerprint == target_fingerprint {
        return Some(OwnedCf(unsafe { CFRetain(element) }));
    }
    if depth >= config.max_depth {
        return None;
    }
    let children = copy_attribute(element, "AXChildren")?;
    let count = unsafe { CFArrayGetCount(children.0) }.max(0);
    for ordinal in 0..count {
        let child = unsafe { CFArrayGetValueAtIndex(children.0, ordinal) };
        if !child.is_null()
            && let Some(found) = find_element(
                child,
                Some(&current_fingerprint),
                depth + 1,
                ordinal as usize,
                target_fingerprint,
                config,
                visited,
            )
        {
            return Some(found);
        }
    }
    None
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
        UiSemanticAction::SetValue { .. } => attribute_settable(element, "AXValue"),
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

fn element_identity(
    element: AxUiElementRef,
    parent: Option<&str>,
    ordinal: usize,
    config: &WalkConfig,
) -> String {
    let role = attribute_string(element, "AXRole").unwrap_or_else(|| "AXUnknown".into());
    let subrole = attribute_string(element, "AXSubrole").unwrap_or_default();
    let (role, _) = bounded_string(if subrole.is_empty() {
        role
    } else {
        format!("{role}/{subrole}")
    });
    let identifier = attribute_string(element, "AXIdentifier").unwrap_or_default();
    fingerprint(
        parent,
        ordinal,
        config.process_id,
        config.process_started_at,
        &role,
        &identifier,
    )
}

fn walk(
    element: AxUiElementRef,
    parent: Option<(Option<u32>, String)>,
    depth: u16,
    sibling_ordinal: usize,
    inside_menu: bool,
    within_window: bool,
    config: &WalkConfig,
    state: &mut WalkState,
    output: &mut Vec<CollectedUiNode>,
) {
    if config.element_only && state.found_menu_window {
        return;
    }
    if state.visited >= 4096 || Instant::now() >= config.deadline {
        state.truncated = true;
        return;
    }
    state.visited += 1;
    if unsafe { AXUIElementSetMessagingTimeout(element, AX_MESSAGE_TIMEOUT_SECONDS) } != AX_SUCCESS
    {
        state.truncated = true;
        return;
    }

    let inside_menu =
        inside_menu || attribute_string(element, "AXRole").is_some_and(|role| is_menu_role(&role));
    if config.scope == UiInspectScope::Content && inside_menu {
        return;
    }
    let within_window = within_window
        || config.menu_window.as_ref().is_some_and(|target| {
            element_identity(
                element,
                parent.as_ref().map(|(_, fingerprint)| fingerprint.as_str()),
                sibling_ordinal,
                config,
            ) == *target
        });
    state.found_menu_window |= within_window;
    let selected = config.menu_window.is_none() || within_window;
    let emit = selected && (config.scope != UiInspectScope::Menus || inside_menu);
    if output.len() >= config.max_nodes || Instant::now() >= config.deadline {
        state.truncated = true;
        return;
    }
    let (index, fingerprint) = if emit {
        let (node, strings_truncated) = read_node(
            element,
            parent.as_ref().map(|(_, fingerprint)| fingerprint.as_str()),
            parent.as_ref().and_then(|(index, _)| *index),
            sibling_ordinal,
            config,
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
                return;
            }
            state.encoded_bytes += encoded_bytes;
            state.truncated |= strings_truncated;
            let index = output.len() as u32;
            let fingerprint = node.fingerprint.clone();
            output.push(node);
            (Some(index), fingerprint)
        }
    } else {
        (
            None,
            element_identity(
                element,
                parent.as_ref().map(|(_, fingerprint)| fingerprint.as_str()),
                sibling_ordinal,
                config,
            ),
        )
    };

    if selected && config.element_only {
        return;
    }
    let Some(children) = copy_attribute(element, "AXChildren") else {
        return;
    };
    let count = unsafe { CFArrayGetCount(children.0) }.max(0) as usize;
    if depth >= config.max_depth {
        state.truncated |= count > 0;
        return;
    }
    for ordinal in 0..count {
        if config.element_only && state.found_menu_window {
            return;
        }
        if output.len() >= config.max_nodes || Instant::now() >= config.deadline {
            state.truncated = true;
            return;
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
        );
    }
}

fn read_node(
    element: AxUiElementRef,
    parent_fingerprint: Option<&str>,
    parent_index: Option<u32>,
    sibling_ordinal: usize,
    config: &WalkConfig,
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
            .or_else(|| attribute_string(element, "AXDescription"))
            .unwrap_or_default();
        let (value, truncated) = bounded_string(raw);
        ((!value.is_empty()).then_some(value), truncated)
    };
    let (value, value_truncated) = if is_protected {
        (None, false)
    } else {
        let (value, truncated) =
            bounded_string(attribute_string(element, "AXValue").unwrap_or_default());
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
        if attribute_settable(element, "AXValue") {
            supported_actions.push(UiSemanticActionKind::SetValue);
        }
        if attribute_settable(element, "AXFocused") {
            supported_actions.push(UiSemanticActionKind::Focus);
        }
    }
    let fingerprint = fingerprint(
        parent_fingerprint,
        sibling_ordinal,
        config.process_id,
        config.process_started_at,
        &role,
        &identifier,
    );
    (
        CollectedUiNode {
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
    let status = unsafe { AXUIElementPerformAction(element, action.0) };
    if status == AX_SUCCESS {
        Ok(())
    } else {
        Err(failure(
            AgentErrorKind::Internal,
            "the Accessibility action was rejected by the target application",
            false,
        ))
    }
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
    let status = unsafe { AXUIElementSetAttributeValue(element, attribute.0, value) };
    if status == AX_SUCCESS {
        Ok(())
    } else {
        Err(failure(
            AgentErrorKind::Internal,
            "the Accessibility attribute update was rejected by the target application",
            false,
        ))
    }
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

fn fingerprint(
    parent: Option<&str>,
    sibling_ordinal: usize,
    process_id: u32,
    process_started_at: u64,
    role: &str,
    identifier: &str,
) -> String {
    let mut hasher = Sha256::new();
    let parent = parent.unwrap_or("root").as_bytes();
    hasher.update(parent.len().to_le_bytes());
    hasher.update(parent);
    hasher.update(sibling_ordinal.to_le_bytes());
    hasher.update(process_id.to_le_bytes());
    hasher.update(process_started_at.to_le_bytes());
    hasher.update(role.len().to_le_bytes());
    hasher.update(role.as_bytes());
    hasher.update(identifier.len().to_le_bytes());
    hasher.update(identifier.as_bytes());
    format!("{:x}", hasher.finalize())
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
    fn fingerprints_bind_process_incarnation_and_parent() {
        let first = fingerprint(Some("parent-a"), 1, 4, 8, "AXButton", "id");
        let second = fingerprint(Some("parent-b"), 1, 4, 8, "AXButton", "id");
        let restarted = fingerprint(Some("parent-a"), 1, 4, 9, "AXButton", "id");
        assert_ne!(first, second);
        assert_ne!(first, restarted);
        assert_ne!(
            fingerprint(Some("parent-a"), 1, 4, 8, "AB", "C"),
            fingerprint(Some("parent-a"), 1, 4, 8, "A", "BC")
        );
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

        let root = unsafe { AXUIElementCreateApplication(restarted_process_id as libc::pid_t) };
        assert!(!root.is_null(), "restarted Calculator Accessibility root");
        let root = OwnedCf(root);
        set_messaging_timeout(root.0).expect("bound restarted Calculator AX messaging");
        let config = WalkConfig {
            query: None,
            element_only: false,
            menu_window: None,
            scope: UiInspectScope::All,
            process_id: restarted_process_id,
            process_started_at: restarted_at,
            max_depth: 16,
            max_nodes: 1_024,
            max_bytes: usize::MAX,
            deadline: Instant::now() + HARD_DEADLINE,
        };
        let mut visited = 0;
        assert!(
            find_element(
                root.0,
                None,
                0,
                0,
                &stale_fingerprint,
                &config,
                &mut visited,
            )
            .is_none(),
            "an AX reference fingerprint from the old process must not resolve in the restarted process"
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
