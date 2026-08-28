//! Bounded macOS Accessibility observation for the active Aqua session.

use std::ffi::{CStr, CString, c_char, c_void};
use std::time::{Duration, Instant};

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
    encoded_bytes: usize,
    truncated: bool,
}

struct WalkConfig {
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
    if !crate::macos_permissions::probe().accessibility {
        return Err(failure(
            AgentErrorKind::PermissionDenied,
            "macOS Accessibility permission is required for semantic UI inspection",
            false,
        ));
    }
    let current = frontmost_application()?;
    if current.process_id != expected_process_id || current.image_path != expected_image_path {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the foreground application changed during Accessibility inspection",
            true,
        ));
    }
    let root = unsafe { AXUIElementCreateApplication(expected_process_id as libc::pid_t) };
    if root.is_null() {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the foreground application has no Accessibility root",
            true,
        ));
    }
    let root = OwnedCf(root);
    set_messaging_timeout(root.0)?;
    let config = WalkConfig {
        process_id: expected_process_id,
        process_started_at: process_start(expected_process_id)?,
        max_depth,
        max_nodes: max_nodes as usize,
        max_bytes: max_bytes as usize,
        deadline: Instant::now() + HARD_DEADLINE,
    };
    let mut state = WalkState::default();
    let mut nodes = Vec::new();
    walk(root.0, None, 0, 0, &config, &mut state, &mut nodes);
    Ok(CollectedUiTree {
        nodes,
        truncated: state.truncated,
    })
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
            Ok(AppliedUiAction {
                changed: true,
                verified: false,
                summary: "Accessibility action was accepted, but its application effect has no generic read-back".into(),
            })
        }
        UiSemanticAction::Select => {
            set_bool_attribute(element.0, "AXSelected", true)?;
            verify_bool_attribute(element.0, "AXSelected", true)?;
            Ok(AppliedUiAction {
                changed: true,
                verified: true,
                summary: "Accessibility selection was read back from the target element".into(),
            })
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
            if attribute_toggle_state(element.0) != Some(*desired) {
                return Err(failure(
                    AgentErrorKind::Internal,
                    "the Accessibility toggle read-back did not match the requested state",
                    false,
                ));
            }
            Ok(AppliedUiAction {
                changed: before != *desired,
                verified: true,
                summary: "Accessibility toggle state was read back from the target element".into(),
            })
        }
        UiSemanticAction::Focus => {
            set_bool_attribute(element.0, "AXFocused", true)?;
            verify_bool_attribute(element.0, "AXFocused", true)?;
            Ok(AppliedUiAction {
                changed: true,
                verified: true,
                summary: "Accessibility focus was read back from the target element".into(),
            })
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
            if attribute_string(element.0, "AXValue").as_deref() != Some(value.as_str()) {
                return Err(failure(
                    AgentErrorKind::Internal,
                    "the Accessibility value read-back did not match the requested value",
                    false,
                ));
            }
            Ok(AppliedUiAction {
                changed: true,
                verified: true,
                summary: "Accessibility value was read back from the target element".into(),
            })
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
    let current = frontmost_application()?;
    if current.process_id != expected_process_id || current.image_path != expected_image_path {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the foreground application changed before the Accessibility action",
            false,
        ));
    }
    let root = unsafe { AXUIElementCreateApplication(expected_process_id as libc::pid_t) };
    if root.is_null() {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "the foreground application has no Accessibility root",
            false,
        ));
    }
    let root = OwnedCf(root);
    set_messaging_timeout(root.0)?;
    let config = WalkConfig {
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

fn walk(
    element: AxUiElementRef,
    parent: Option<(u32, String)>,
    depth: u16,
    sibling_ordinal: usize,
    config: &WalkConfig,
    state: &mut WalkState,
    output: &mut Vec<CollectedUiNode>,
) {
    if output.len() >= config.max_nodes || Instant::now() >= config.deadline {
        state.truncated = true;
        return;
    }
    if unsafe { AXUIElementSetMessagingTimeout(element, AX_MESSAGE_TIMEOUT_SECONDS) } != AX_SUCCESS
    {
        state.truncated = true;
        return;
    }
    let (node, strings_truncated) = read_node(
        element,
        parent.as_ref().map(|(_, fingerprint)| fingerprint.as_str()),
        parent.as_ref().map(|(index, _)| *index),
        sibling_ordinal,
        config,
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

    let Some(children) = copy_attribute(element, "AXChildren") else {
        return;
    };
    let count = unsafe { CFArrayGetCount(children.0) }.max(0) as usize;
    if depth >= config.max_depth {
        state.truncated |= count > 0;
        return;
    }
    for ordinal in 0..count {
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

fn frontmost_application() -> Result<ObservedApplication, AgentError> {
    autoreleasepool(|_| unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let application: *mut AnyObject = msg_send![workspace, frontmostApplication];
        if application.is_null() {
            return Err(failure(
                AgentErrorKind::SessionUnavailable,
                "the macOS Aqua session has no frontmost application",
                true,
            ));
        }
        let process_id: i32 = msg_send![application, processIdentifier];
        let executable_url: *mut AnyObject = msg_send![application, executableURL];
        if process_id <= 0 || executable_url.is_null() {
            return Err(failure(
                AgentErrorKind::SessionUnavailable,
                "cannot resolve the frontmost macOS application identity",
                true,
            ));
        }
        let path: *mut AnyObject = msg_send![executable_url, path];
        let utf8: *const c_char = msg_send![path, UTF8String];
        if utf8.is_null() {
            return Err(failure(
                AgentErrorKind::SessionUnavailable,
                "cannot resolve the frontmost macOS application path",
                true,
            ));
        }
        Ok(ObservedApplication {
            process_id: process_id as u32,
            image_path: CStr::from_ptr(utf8).to_string_lossy().into_owned(),
        })
    })
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

fn verify_bool_attribute(
    element: AxUiElementRef,
    attribute: &str,
    expected: bool,
) -> Result<(), AgentError> {
    if attribute_bool(element, attribute) == Some(expected) {
        Ok(())
    } else {
        Err(failure(
            AgentErrorKind::Internal,
            "the Accessibility boolean read-back did not match the requested value",
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
        assert!(!result.verified);
    }
}
