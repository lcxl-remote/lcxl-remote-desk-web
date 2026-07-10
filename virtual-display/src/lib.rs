//! Virtual display abstraction.
//!
//! Defines a platform-agnostic contract for creating and controlling
//! virtual monitors. The Windows backend drives a Microsoft Indirect
//! Display Driver (IDD) via a named-pipe control channel (the real
//! `SwDeviceCreate` + driver-pipe + `ChangeDisplaySettings` pipeline);
//! other platforms fulfil the contract with a permanent `NotSupported`
//! stub.

use serde::{Deserialize, Serialize};

#[cfg(not(target_os = "windows"))]
pub mod unsupported;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(not(target_os = "windows"))]
pub use unsupported::{
    ExclusiveLayout, PhysicalDisplaySnapshot, PromptController, PromptWaiter, enter_exclusive,
    enumerate_active_displays_for_diagnostics, leave_exclusive,
    log_active_displays_for_diagnostics, show_pre_detach_prompt, snapshot_layout,
};
/// Exclusive-mode helpers re-exported so the worker `ExclusiveCoordinator`
/// can import them via a single path on Windows. Other platforms get
/// permanent stubs from [`unsupported`].
#[cfg(target_os = "windows")]
pub use windows::physical::{
    ExclusiveLayout, PhysicalDisplaySnapshot, enter_exclusive,
    enumerate_active_displays_for_diagnostics, leave_exclusive,
    log_active_displays_for_diagnostics, snapshot_layout,
};
#[cfg(target_os = "windows")]
pub use windows::prompt::{PromptController, PromptWaiter, show_pre_detach_prompt};

/// Lifecycle owner: held by the LocalSystem daemon. Creating a handle
/// allocates the OS-level virtual monitor resource; dropping the handle
/// releases it.
pub trait VirtualDisplayLifecycle: Send + Sync {
    fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError>;
}

/// Opaque platform-specific payload behind [`VirtualDisplayHandle`].
/// `Drop` releases the underlying OS resource.
pub trait VirtualDisplayHandleInner: Send + Sync {}

/// Owned reference to a live virtual monitor. The handle keeps the OS
/// resource alive for as long as it lives.
///
/// `instance_id` carries the **PnP device instance id** assigned by the
/// OS during `SwDeviceCreate` (e.g.
/// `SWD\LcxlVirtualDisplay\LcxlVirtualDisplay`). It is **not** a GDI
/// display name (`\\.\DISPLAYn`); the LocalSystem daemon cannot resolve
/// the instance id locally because `EnumDisplayDevicesW` is
/// thread-desktop-bound and returns an empty enumeration from Session 0.
/// The daemon therefore forwards the instance id to a user-session
/// worker via IPC, and the worker calls [`resolve_display_name`] to
/// turn it into a GDI display name suitable for
/// `ChangeDisplaySettingsExW`.
pub struct VirtualDisplayHandle {
    pub instance_id: String,
    #[allow(dead_code)]
    inner: Box<dyn VirtualDisplayHandleInner>,
}

impl VirtualDisplayHandle {
    /// Public constructor so platform implementations (including future
    /// out-of-crate impls) can build a handle.
    pub fn new(instance_id: String, inner: Box<dyn VirtualDisplayHandleInner>) -> Self {
        Self { instance_id, inner }
    }
}

impl std::fmt::Debug for VirtualDisplayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualDisplayHandle")
            .field("instance_id", &self.instance_id)
            .finish_non_exhaustive()
    }
}

/// Runtime controller: lives in the user-session worker. Pushes a new
/// mode through the driver pipe and commits it via the windowing system.
pub trait VirtualDisplayController: Send + Sync {
    fn set_mode(
        &self,
        display_name: &str,
        mode: VirtualDisplayMode,
    ) -> Result<VirtualDisplayMode, VirtualDisplayError>;
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct VirtualDisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum VirtualDisplayError {
    #[error("platform not supported")]
    NotSupported,
    #[error("invalid mode: {0}")]
    InvalidMode(String),
    #[error("driver pipe IO failed: {0}")]
    PipeIo(String),
    #[error("driver returned NTSTATUS={0:#x}")]
    DriverFailed(u32),
    #[error("ChangeDisplaySettings failed: {0}")]
    Cds(String),
    #[error("SwDeviceCreate failed: {0}")]
    DeviceCreate(String),
    #[error("not attached")]
    NotAttached,
}

const MIN_DIMENSION: u32 = 640;
const MAX_DIMENSION: u32 = 7680;
// `ALIGNMENT` started life at 8 (a conservative compromise: H.264 sub-MB
// is 8 px, AVC macroblock 16 px, GPU cache lines often 16 BGRA px). It
// was the daemon's worry-bead — not an IDD/IddCx protocol requirement
// (the C++ driver never validated alignment) and not a YUV requirement
// (the capture → YUV path uses `div_ceil(2)` and survives odd
// dimensions, see `web/capture-engine/src/video_encoder/yuv_utils.rs`).
// Adaptive resolution needs pixel-for-pixel viewport mapping so we
// dropped the alignment to 1. The const stays so a future regression
// that re-introduces a non-trivial alignment requirement is a single
// line change.
const ALIGNMENT: u32 = 1;
const ALLOWED_REFRESH: &[u32] = &[24, 30, 50, 60, 75, 90, 120, 144, 165, 240];

/// Validate a requested mode against the conservative bounds the daemon
/// is willing to forward to the driver. Defense-in-depth: the router
/// calls this before sending the IPC, and platform controllers re-check
/// before issuing the driver request.
pub fn validate_mode(mode: VirtualDisplayMode) -> Result<(), VirtualDisplayError> {
    if mode.width == 0 || mode.height == 0 || mode.refresh_hz == 0 {
        return Err(VirtualDisplayError::InvalidMode(format!(
            "zero dimension: width={} height={} refresh_hz={}",
            mode.width, mode.height, mode.refresh_hz
        )));
    }
    if !(MIN_DIMENSION..=MAX_DIMENSION).contains(&mode.width) {
        return Err(VirtualDisplayError::InvalidMode(format!(
            "width {} out of range [{MIN_DIMENSION}, {MAX_DIMENSION}]",
            mode.width
        )));
    }
    if !(MIN_DIMENSION..=MAX_DIMENSION).contains(&mode.height) {
        return Err(VirtualDisplayError::InvalidMode(format!(
            "height {} out of range [{MIN_DIMENSION}, {MAX_DIMENSION}]",
            mode.height
        )));
    }
    if !mode.width.is_multiple_of(ALIGNMENT) || !mode.height.is_multiple_of(ALIGNMENT) {
        return Err(VirtualDisplayError::InvalidMode(format!(
            "width/height must be a multiple of {ALIGNMENT} (got {}x{})",
            mode.width, mode.height
        )));
    }
    if !ALLOWED_REFRESH.contains(&mode.refresh_hz) {
        return Err(VirtualDisplayError::InvalidMode(format!(
            "refresh_hz {} not in allowed set {ALLOWED_REFRESH:?}",
            mode.refresh_hz
        )));
    }
    Ok(())
}

/// Named pipe path of the driver's control endpoint. Shared between the
/// Rust controller (client) and the C++ driver (server).
pub const PIPE_NAME: &str = r"\\.\pipe\LcxlVirtualDisplay";

// ───── Driver pipe protocol (shared with C++ driver via JSON schema) ─────
//
// Framing: 4-byte little-endian u32 length header + UTF-8 JSON body. The
// length excludes the header bytes. Both sides MUST reject any frame whose
// length exceeds `DRIVER_MAX_MESSAGE_SIZE` to protect the driver from
// out-of-memory attacks if the channel ever desynchronises.
//
// Schema additions are forward-compatible (new commands / response fields
// are tolerated by older peers thanks to `serde`'s default skip-unknown
// behaviour for structs and the explicit `command` tag for requests). A
// breaking change to the wire format MUST bump `DRIVER_PROTOCOL_VERSION`
// on both sides — runtime version negotiation is not yet implemented.

/// Request envelope sent from the worker to the driver.
///
/// Serialised as `{"command": "<snake_case>", "params": { ... }}`. Adding
/// a new variant (e.g. `GetModes`) is forward-compatible: drivers built
/// against an older schema reject unknown commands with status 100.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", content = "params", rename_all = "snake_case")]
pub enum DriverRequest {
    /// Push a new target mode through the IDD monitor. The driver applies
    /// the mode list via `IddCxMonitorUpdateModes` and reports the actual
    /// mode it activated in `DriverResponse.data.applied_mode` — drivers
    /// MAY snap to the closest supported mode.
    SetMode {
        width: u32,
        height: u32,
        refresh_hz: u32,
    },
}

/// Response envelope returned by the driver.
///
/// - `status_code` is 0 on success.
/// - `status_code` in `1..1000` is reserved for protocol / validation
///   errors raised by the driver itself (100 = unknown command,
///   101 = malformed framing / payload too large, 102 = invalid JSON,
///   103 = missing or zero required field).
/// - `status_code` outside that range is an NTSTATUS from IddCx (cast
///   `NTSTATUS` → `i32`); the worker translates these into
///   [`VirtualDisplayError::DriverFailed`].
///
/// `error` carries a human-readable message and is populated whenever
/// `success` is false. `data` and `details` are command-specific
/// structured payloads; they are omitted from the wire when `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverResponse {
    pub success: bool,
    pub status_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl DriverResponse {
    /// Build a success response with optional structured data payload.
    pub fn success(data: Option<serde_json::Value>) -> Self {
        Self {
            success: true,
            status_code: 0,
            data,
            error: None,
            details: None,
        }
    }

    /// Build a failure response with an `status_code` (protocol-defined
    /// or NTSTATUS) and message.
    pub fn failure(status_code: i32, error: impl Into<String>) -> Self {
        Self {
            success: false,
            status_code,
            data: None,
            error: Some(error.into()),
            details: None,
        }
    }
}

/// Schema version for the driver named-pipe protocol. Bump whenever the
/// wire format changes in a way an older peer cannot parse; both Rust
/// and C++ sides MUST be rebuilt together. Runtime negotiation is not
/// yet implemented — for now we rely on lockstep deployment of the
/// driver and the user-mode controller.
pub const DRIVER_PROTOCOL_VERSION: u32 = 1;

/// Hard cap on a single framed message (header + body). 64 KiB is far
/// above anything the current command set needs and protects the driver
/// from OOM if the channel ever desynchronises.
pub const DRIVER_MAX_MESSAGE_SIZE: u32 = 64 * 1024;

pub fn lifecycle_provider() -> Box<dyn VirtualDisplayLifecycle> {
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsLifecycle::new())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Box::new(unsupported::UnsupportedLifecycle)
    }
}

pub fn controller_provider() -> Box<dyn VirtualDisplayController> {
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsController::new())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Box::new(unsupported::UnsupportedController)
    }
}

/// Resolve the OS-level PnP instance id (e.g.
/// `SWD\LcxlVirtualDisplay\LcxlVirtualDisplay`) returned by
/// [`VirtualDisplayLifecycle::create`] to a GDI display name
/// (`\\.\DISPLAYn`) usable by `ChangeDisplaySettingsExW`.
///
/// **Caller scope (Windows)**: must run inside the interactive user
/// session. `EnumDisplayDevicesW` is thread-desktop-bound and returns
/// an empty set when called from the LocalSystem service desktop
/// (Session 0), even when the virtual monitor is in fact registered.
/// The daemon therefore hands the instance id to its user-session
/// worker via IPC, and the worker calls this function.
///
/// Returns [`VirtualDisplayError::NotSupported`] on non-Windows builds.
pub fn resolve_display_name(instance_id: &str) -> Result<String, VirtualDisplayError> {
    #[cfg(target_os = "windows")]
    {
        windows::sw_device::find_display_name(instance_id)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = instance_id;
        Err(VirtualDisplayError::NotSupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_mode_accepts_common_resolutions() {
        for (w, h, r) in [
            (640, 640, 60),
            (1280, 720, 60),
            (1920, 1080, 60),
            (1920, 1080, 144),
            (2560, 1440, 165),
            (3840, 2160, 60),
            (7680, 4320, 60),
        ] {
            validate_mode(VirtualDisplayMode {
                width: w,
                height: h,
                refresh_hz: r,
            })
            .unwrap_or_else(|e| panic!("expected valid {w}x{h}@{r}: {e}"));
        }
    }

    #[test]
    fn validate_mode_rejects_zero() {
        for m in [
            VirtualDisplayMode {
                width: 0,
                height: 720,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 1280,
                height: 0,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 1280,
                height: 720,
                refresh_hz: 0,
            },
        ] {
            assert!(matches!(
                validate_mode(m),
                Err(VirtualDisplayError::InvalidMode(_))
            ));
        }
    }

    #[test]
    fn validate_mode_rejects_out_of_range() {
        for m in [
            VirtualDisplayMode {
                width: 320,
                height: 720,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 8000,
                height: 720,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 1280,
                height: 320,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 1280,
                height: 7688,
                refresh_hz: 60,
            },
        ] {
            assert!(
                matches!(validate_mode(m), Err(VirtualDisplayError::InvalidMode(_))),
                "expected out-of-range error for {m:?}"
            );
        }
    }

    #[test]
    fn validate_mode_rejects_odd_refresh() {
        let m = VirtualDisplayMode {
            width: 1280,
            height: 720,
            refresh_hz: 100,
        };
        assert!(matches!(
            validate_mode(m),
            Err(VirtualDisplayError::InvalidMode(_))
        ));
    }

    /// `ALIGNMENT` is now 1, so odd dimensions must validate (provided
    /// they stay within the `[MIN_DIMENSION, MAX_DIMENSION]` range).
    /// Adaptive resolution depends on this — the browser hook does not
    /// round to any boundary, only clamps to that range. This test
    /// guards against a regression that re-raises `ALIGNMENT` to
    /// 2 / 8 / 16 without revisiting the adaptive path.
    #[test]
    fn validate_mode_accepts_odd_dimensions() {
        for m in [
            VirtualDisplayMode {
                width: 1003,
                height: 641, // 641 = MIN_DIMENSION + 1, deliberately odd
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 1281,
                height: 721,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: MIN_DIMENSION + 1,
                height: MIN_DIMENSION + 3,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                // Pin pixel-for-pixel viewport mapping: an
                // exactly-on-boundary even-on-one-axis-odd-on-other
                // case that used to be rejected when ALIGNMENT=8.
                width: 1920,
                height: 1077,
                refresh_hz: 60,
            },
        ] {
            assert!(
                validate_mode(m).is_ok(),
                "expected odd dimensions to validate after ALIGNMENT=1, got error for {m:?}"
            );
        }
    }

    /// The other validation gates (zero / out-of-range / refresh in allow
    /// set) must still fire — relaxing ALIGNMENT to 1 must not silently
    /// open the door for the rest of the protocol surface.
    #[test]
    fn validate_mode_still_rejects_zero_and_out_of_range() {
        let cases = [
            VirtualDisplayMode {
                width: 0,
                height: 720,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 1280,
                height: 0,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 1280,
                height: 720,
                refresh_hz: 0,
            },
            VirtualDisplayMode {
                width: MIN_DIMENSION - 1,
                height: 720,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: MAX_DIMENSION + 1,
                height: 720,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 1280,
                height: 720,
                refresh_hz: 100, // not in ALLOWED_REFRESH
            },
        ];
        for m in cases {
            assert!(
                matches!(validate_mode(m), Err(VirtualDisplayError::InvalidMode(_))),
                "expected error after ALIGNMENT=1 for {m:?}"
            );
        }
    }

    #[test]
    fn pipe_name_constant_unchanged() {
        assert_eq!(PIPE_NAME, r"\\.\pipe\LcxlVirtualDisplay");
    }

    #[test]
    fn mode_serde_roundtrip() {
        let m = VirtualDisplayMode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let j = serde_json::to_string(&m).unwrap();
        let back: VirtualDisplayMode = serde_json::from_str(&j).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn driver_request_set_mode_serde_roundtrip() {
        let req = DriverRequest::SetMode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let json = serde_json::to_value(&req).unwrap();
        // Verify exact wire shape — this is the contract with the C++ driver.
        assert_eq!(
            json,
            serde_json::json!({
                "command": "set_mode",
                "params": {
                    "width": 1920,
                    "height": 1080,
                    "refresh_hz": 60,
                }
            })
        );
        let back: DriverRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn driver_request_rejects_unknown_command() {
        let raw = serde_json::json!({
            "command": "do_something_else",
            "params": {}
        });
        let err = serde_json::from_value::<DriverRequest>(raw).unwrap_err();
        assert!(
            err.to_string().contains("do_something_else")
                || err.to_string().contains("unknown variant"),
            "expected unknown-variant error, got: {err}"
        );
    }

    #[test]
    fn driver_response_success_serde_roundtrip() {
        let resp = DriverResponse::success(Some(serde_json::json!({
            "applied_mode": { "width": 1280, "height": 720, "refresh_hz": 60 }
        })));
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], serde_json::Value::Bool(true));
        assert_eq!(json["status_code"], serde_json::json!(0));
        assert_eq!(
            json["data"]["applied_mode"]["width"],
            serde_json::json!(1280)
        );
        // None fields must be skipped on the wire.
        assert!(json.get("error").is_none());
        assert!(json.get("details").is_none());
        let back: DriverResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp, back);
    }

    #[test]
    fn driver_response_failure_with_error_serde_roundtrip() {
        let mut resp = DriverResponse::failure(103, "missing width");
        resp.details = Some(serde_json::json!({ "field": "width" }));
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], serde_json::Value::Bool(false));
        assert_eq!(json["status_code"], serde_json::json!(103));
        assert_eq!(json["error"], serde_json::json!("missing width"));
        assert_eq!(json["details"]["field"], serde_json::json!("width"));
        // Successful payload field must be skipped.
        assert!(json.get("data").is_none());
        let back: DriverResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp, back);
    }

    #[test]
    fn driver_response_skips_none_fields_on_serialization() {
        let resp = DriverResponse {
            success: true,
            status_code: 0,
            data: None,
            error: None,
            details: None,
        };
        let s = serde_json::to_string(&resp).unwrap();
        // Wire format must only contain success + status_code when other
        // fields are None — verifies serde(skip_serializing_if).
        assert!(!s.contains("data"));
        assert!(!s.contains("error"));
        assert!(!s.contains("details"));
        assert!(s.contains("\"success\":true"));
        assert!(s.contains("\"status_code\":0"));
    }

    #[test]
    fn driver_response_tolerates_unknown_fields_for_forward_compat() {
        // A future driver may add new response fields. Older clients must
        // still parse the known ones rather than rejecting the message.
        let json = serde_json::json!({
            "success": true,
            "status_code": 0,
            "data": { "applied_mode": { "width": 1024, "height": 768, "refresh_hz": 60 } },
            "future_field": "this should not break parsing"
        });
        let resp: DriverResponse = serde_json::from_value(json).unwrap();
        assert!(resp.success);
        assert_eq!(resp.status_code, 0);
        assert_eq!(
            resp.data.unwrap()["applied_mode"]["width"],
            serde_json::json!(1024)
        );
    }

    /// Non-Windows builds keep the permanent stub: a daemon (or test
    /// harness) calling `resolve_display_name` from Linux / macOS must
    /// observe `NotSupported`, not a silent empty string or a panic.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn resolve_display_name_returns_not_supported_on_non_windows() {
        let err = resolve_display_name("SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay")
            .expect_err("non-Windows must reject");
        assert!(matches!(err, VirtualDisplayError::NotSupported));
    }

    #[test]
    fn driver_protocol_version_and_max_size_constants() {
        // These constants pin the wire contract — bumping them is a
        // breaking change shared with the C++ driver. The test exists so
        // a casual edit ("let's bump to 2") shows up as a failing test
        // and forces the author to acknowledge the cross-repo blast.
        assert_eq!(DRIVER_PROTOCOL_VERSION, 1);
        assert_eq!(DRIVER_MAX_MESSAGE_SIZE, 64 * 1024);
    }
}
