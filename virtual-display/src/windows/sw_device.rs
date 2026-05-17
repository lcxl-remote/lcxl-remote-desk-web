//! PnP software-device lifecycle for the virtual display.
//!
//! Materialises the lcxl IDD as a software device via `SwDeviceCreate`,
//! waits for the OS to confirm creation, and holds the handle so
//! `SwDeviceClose` fires on drop. Modelled on the PoC at
//! `pocs/poc-indirect-display/src/device.rs`, but trimmed to the
//! signatures the production trait needs.
//!
//! GDI display-name resolution (`\\.\DISPLAYn`) does **not** live here
//! because `EnumDisplayDevicesW` is thread-desktop-bound and returns an
//! empty enumeration when called from the LocalSystem service desktop
//! (Session 0). The daemon hands the PnP instance id to a user-session
//! worker via IPC; the worker then invokes [`find_display_name`] (via
//! [`crate::resolve_display_name`]) from inside the interactive desktop
//! where the GDI walk succeeds.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::debug;
use windows::Win32::Devices::Enumeration::Pnp::{
    HSWDEVICE, SW_DEVICE_CREATE_INFO, SWDeviceCapabilitiesDriverRequired,
    SWDeviceCapabilitiesRemovable, SWDeviceCapabilitiesSilentInstall, SwDeviceCreate,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE as WHANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Gdi::{DISPLAY_DEVICEW, EnumDisplayDevicesW};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::core::{Owned, PCWSTR};

use crate::{VirtualDisplayError, VirtualDisplayHandleInner};

/// Hardware ID published by the lcxl production virtual-display INF
/// (`enterprise/virtual-display-driver/`). Production installs MUST
/// advertise this exact string in their INF, or the OS will not match
/// a driver and `SwDeviceCreate` reports no-match. Deliberately
/// distinct from the PoC HW ID `LcxlIddSampleDriver` so the two INFs
/// can coexist on a developer test machine without overlapping.
pub const LCXL_IDD_HARDWARE_ID: &str = "LcxlVirtualDisplay";
/// Instance ID requested when materialising the device. Stable across
/// restarts so the PnP manager can find the same node.
pub const LCXL_IDD_INSTANCE_ID: &str = "LcxlVirtualDisplay";
/// Friendly description shown by the OS in Device Manager.
pub const LCXL_IDD_DESCRIPTION: &str = "Lcxl Virtual Display";

/// Maximum time we are willing to wait for the OS to confirm the device
/// has been installed before giving up. Driver installations on a clean
/// machine routinely take several seconds.
const DEFAULT_CREATE_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum `idevnum` we will try when scanning for the new GDI display.
/// The OS does not document an upper bound but in practice no machine
/// has more than a handful of display adapters. 64 is a defensive cap.
const MAX_DISPLAY_ENUM_INDEX: u32 = 64;

/// Owned handle returned by [`create_virtual_display`]. Dropping it
/// closes `HSWDEVICE` via `SwDeviceClose`, which tears the virtual
/// monitor back down.
///
/// `HSWDEVICE` is a `*mut c_void` newtype; we assert `Send + Sync`
/// because the daemon supervisor stores the handle behind an `Arc` and
/// may hand it across `tokio::task::spawn_blocking` boundaries. The OS
/// guarantees `SwDeviceClose` is thread-safe; the handle is effectively
/// an opaque reference owned by the lifecycle.
pub struct SwDeviceHandle {
    // Held only for Drop side effect: dropping Owned<HSWDEVICE> invokes
    // SwDeviceClose, which tears the virtual monitor back down. Reading
    // the raw handle anywhere in production code is wrong.
    #[allow(dead_code)]
    handle: Owned<HSWDEVICE>,
    instance_id: String,
}

// SAFETY: HSWDEVICE is a PnP-owned opaque handle; the kernel-side state
// it points at is thread-safe for the only API we call on it
// (`SwDeviceClose`). Sending across threads is safe.
unsafe impl Send for SwDeviceHandle {}
// SAFETY: see Send rationale; concurrent shared access is also safe
// because nothing in this crate calls into the handle after creation.
unsafe impl Sync for SwDeviceHandle {}

impl SwDeviceHandle {
    /// Device instance id reported by the PnP manager during creation
    /// (e.g. `ROOT\LCXLIDDSAMPLEDRIVER\0000`). Available for diagnostics
    /// and for cross-checking the result of [`find_display_name`].
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

impl VirtualDisplayHandleInner for SwDeviceHandle {}

/// Encode a single wide-string with a trailing NUL terminator.
fn encode_pcwstr(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// Encode a sequence of strings as PCZZWSTR (each NUL-terminated, list
/// double-NUL-terminated). Required for the hardware/compatible id
/// fields of `SW_DEVICE_CREATE_INFO`.
fn encode_pczzwstr(values: &[&str]) -> Vec<u16> {
    let mut v: Vec<u16> = Vec::new();
    for s in values {
        v.extend(s.encode_utf16());
        v.push(0);
    }
    v.push(0);
    v
}

/// Convert a fixed-size wide-char buffer (e.g. `DISPLAY_DEVICEW::DeviceName`)
/// to a Rust string by trimming at the first NUL.
fn wide_array_to_string(buf: &[u16]) -> String {
    let nul = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..nul])
}

struct CallbackState {
    create_result: windows::core::HRESULT,
    device_instance_id: String,
}

struct CallbackContext {
    event: WHANDLE,
    state: Arc<Mutex<Option<CallbackState>>>,
}

unsafe extern "system" fn creation_callback(
    _hswdevice: HSWDEVICE,
    create_result: windows::core::HRESULT,
    pcontext: *const c_void,
    pszdeviceinstanceid: PCWSTR,
) {
    if pcontext.is_null() {
        return;
    }
    // SAFETY: pcontext is the Box::into_raw value passed by `create`. The
    // Box is reclaimed there after the wait completes.
    let ctx = unsafe { &*(pcontext as *const CallbackContext) };

    let device_instance_id = if pszdeviceinstanceid.is_null() {
        String::new()
    } else {
        // SAFETY: per PCWSTR contract — wide string is NUL-terminated
        // and stays valid for the duration of the callback.
        unsafe { pszdeviceinstanceid.to_string().unwrap_or_default() }
    };

    if let Ok(mut guard) = ctx.state.lock() {
        *guard = Some(CallbackState {
            create_result,
            device_instance_id,
        });
    }
    let _ = unsafe { SetEvent(ctx.event) };
}

/// Create the virtual display software device and return its PnP
/// instance id (e.g. `SWD\LcxlVirtualDisplay\LcxlVirtualDisplay`). The
/// returned [`SwDeviceHandle`] keeps the device alive for as long as it
/// lives.
///
/// This function does **not** resolve the GDI `\\.\DISPLAYn` name —
/// see the module doc-comment for why. Callers wanting the display
/// name must invoke [`crate::resolve_display_name`] from inside the
/// interactive user session.
pub fn create_virtual_display() -> Result<(SwDeviceHandle, String), VirtualDisplayError> {
    create_virtual_display_with_timeout(DEFAULT_CREATE_TIMEOUT)
}

pub fn create_virtual_display_with_timeout(
    timeout: Duration,
) -> Result<(SwDeviceHandle, String), VirtualDisplayError> {
    // Buffers must outlive the SwDeviceCreate call. Keep them on the
    // stack frame and pass raw PCWSTR pointers.
    let instance_id_buf = encode_pcwstr(LCXL_IDD_INSTANCE_ID);
    let hardware_ids = encode_pczzwstr(&[LCXL_IDD_HARDWARE_ID]);
    let compatible_ids = encode_pczzwstr(&[LCXL_IDD_HARDWARE_ID]);
    let description = encode_pcwstr(LCXL_IDD_DESCRIPTION);
    let parent_device_instance = encode_pcwstr("HTREE\\ROOT\\0");
    let enumerator_name = encode_pcwstr(LCXL_IDD_HARDWARE_ID);

    let create_info = SW_DEVICE_CREATE_INFO {
        cbSize: std::mem::size_of::<SW_DEVICE_CREATE_INFO>() as u32,
        pszInstanceId: PCWSTR(instance_id_buf.as_ptr()),
        pszzHardwareIds: PCWSTR(hardware_ids.as_ptr()),
        pszzCompatibleIds: PCWSTR(compatible_ids.as_ptr()),
        pContainerId: std::ptr::null(),
        CapabilityFlags: (SWDeviceCapabilitiesRemovable.0
            | SWDeviceCapabilitiesSilentInstall.0
            | SWDeviceCapabilitiesDriverRequired.0) as u32,
        pszDeviceDescription: PCWSTR(description.as_ptr()),
        pszDeviceLocation: PCWSTR::null(),
        pSecurityDescriptor: std::ptr::null(),
    };

    // SAFETY: CreateEventW returns a manually-reset OS handle we close
    // explicitly after the wait completes.
    let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map_err(|e| VirtualDisplayError::DeviceCreate(format!("CreateEventW: {e}")))?;

    let state: Arc<Mutex<Option<CallbackState>>> = Arc::new(Mutex::new(None));
    let context = Box::new(CallbackContext {
        event,
        state: state.clone(),
    });
    let context_raw = Box::into_raw(context);

    // SAFETY: All wide-string buffers above outlive the call. The
    // callback context is alive until we reclaim the Box after the wait.
    let handle_result = unsafe {
        SwDeviceCreate(
            PCWSTR(enumerator_name.as_ptr()),
            PCWSTR(parent_device_instance.as_ptr()),
            &create_info,
            None,
            Some(creation_callback),
            Some(context_raw as *const c_void),
        )
    };

    let owned = match handle_result {
        Ok(h) => unsafe { Owned::new(h) },
        Err(e) => {
            // The OS does not invoke the callback when SwDeviceCreate
            // itself fails. Reclaim and free our context box.
            unsafe {
                let _ = CloseHandle(event);
                drop(Box::from_raw(context_raw));
            }
            return Err(VirtualDisplayError::DeviceCreate(format!(
                "SwDeviceCreate: {e}"
            )));
        }
    };

    let timeout_ms: u32 = timeout
        .as_millis()
        .try_into()
        .unwrap_or(u32::MAX.saturating_sub(1));
    let wait = unsafe { WaitForSingleObject(event, timeout_ms) };
    // Reclaim the callback context box. SwDeviceCreate fires the
    // callback at most once for a single create operation, so either it
    // already ran (and signalled the event) or it never will.
    let _ = unsafe { Box::from_raw(context_raw) };
    let _ = unsafe { CloseHandle(event) };

    if wait != WAIT_OBJECT_0 {
        return Err(VirtualDisplayError::DeviceCreate(format!(
            "timed out after {timeout:?} waiting for SwDeviceCreate callback"
        )));
    }

    let final_state = state
        .lock()
        .map_err(|e| VirtualDisplayError::DeviceCreate(format!("state lock poisoned: {e}")))?
        .take()
        .ok_or_else(|| {
            VirtualDisplayError::DeviceCreate(
                "creation callback fired without recording state".into(),
            )
        })?;

    if final_state.create_result.is_err() {
        return Err(VirtualDisplayError::DeviceCreate(format!(
            "SwDeviceCreate completion reported failure: {:?}",
            final_state.create_result
        )));
    }

    debug!(
        "SwDeviceCreate completed; instance_id={}",
        final_state.device_instance_id
    );

    // GDI display-name resolution is deferred to the user-session
    // worker (see module doc-comment for the Session 0 isolation
    // rationale). The daemon hands the instance id over IPC and the
    // worker calls `find_display_name` from inside the interactive
    // desktop.
    let instance_id = final_state.device_instance_id.clone();
    Ok((
        SwDeviceHandle {
            handle: owned,
            instance_id: final_state.device_instance_id,
        },
        instance_id,
    ))
}

/// Scan `EnumDisplayDevicesW` for the adapter whose `DeviceID` matches
/// the freshly-created software device's instance id, returning the
/// associated `DeviceName` (`\\.\DISPLAYn`).
///
/// **Caller scope**: must run inside the interactive user session.
/// `EnumDisplayDevicesW` is thread-desktop-bound and returns an empty
/// enumeration when called from Session 0 (LocalSystem service desktop)
/// even when the virtual monitor is in fact registered in the PnP tree.
/// See the module doc-comment for the chain of evidence.
///
/// **Matching strategy**: today this function matches on the
/// [`LCXL_IDD_HARDWARE_ID`] substring rather than the supplied
/// `instance_id` argument. The argument is only embedded in the error
/// message on miss. This is safe under the current "at most one lcxl
/// virtual display per host" assumption (driven by the fixed
/// [`LCXL_IDD_INSTANCE_ID`] constant). When multi-virtual-display is
/// added, this match must be tightened to compare against the supplied
/// `instance_id` instead, otherwise the wrong `\\.\DISPLAYn` could be
/// returned. The substring-on-HW-id choice also smooths over PnP id
/// casing/format differences across Windows versions for the single
/// virtual display case.
pub fn find_display_name(instance_id: &str) -> Result<String, VirtualDisplayError> {
    let needle = LCXL_IDD_HARDWARE_ID.to_ascii_uppercase();
    let mut last_seen: Vec<String> = Vec::new();
    for idevnum in 0u32..MAX_DISPLAY_ENUM_INDEX {
        let mut dev = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: dev is a valid mutable buffer; PCWSTR::null() is allowed
        // for the lpDevice parameter when enumerating top-level adapters.
        let ok = unsafe { EnumDisplayDevicesW(None, idevnum, &mut dev, 0) };
        if !ok.as_bool() {
            break;
        }
        let device_id = wide_array_to_string(&dev.DeviceID);
        let device_name = wide_array_to_string(&dev.DeviceName);
        last_seen.push(format!("{device_name}={device_id}"));
        if device_id.to_ascii_uppercase().contains(&needle) {
            return Ok(device_name);
        }
    }
    Err(VirtualDisplayError::DeviceCreate(format!(
        "no GDI display matches lcxl IDD instance {instance_id}; seen=[{}]",
        last_seen.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_pcwstr_terminates_with_nul() {
        let v = encode_pcwstr("ab");
        assert_eq!(v, vec![b'a' as u16, b'b' as u16, 0]);
    }

    #[test]
    fn encode_pcwstr_empty_string_is_single_nul() {
        assert_eq!(encode_pcwstr(""), vec![0u16]);
    }

    #[test]
    fn encode_pczzwstr_single_string_double_terminates() {
        let v = encode_pczzwstr(&["ab"]);
        assert_eq!(v, vec![b'a' as u16, b'b' as u16, 0, 0]);
    }

    #[test]
    fn encode_pczzwstr_two_strings_each_separate() {
        let v = encode_pczzwstr(&["ab", "cd"]);
        assert_eq!(
            v,
            vec![b'a' as u16, b'b' as u16, 0, b'c' as u16, b'd' as u16, 0, 0]
        );
    }

    #[test]
    fn encode_pczzwstr_empty_list_just_terminator() {
        assert_eq!(encode_pczzwstr(&[]), vec![0u16]);
    }

    #[test]
    fn wide_array_to_string_trims_at_first_nul() {
        let mut buf = [0u16; 8];
        for (i, c) in "Hi".encode_utf16().enumerate() {
            buf[i] = c;
        }
        // Remaining elements are 0 (default), so the trim must stop at
        // the first NUL rather than reading garbage from the tail.
        assert_eq!(wide_array_to_string(&buf), "Hi");
    }

    #[test]
    fn wide_array_to_string_handles_fully_populated_buffer() {
        let mut buf = [0u16; 4];
        for (i, c) in "ABCD".encode_utf16().enumerate() {
            buf[i] = c;
        }
        // No trailing NUL — must consume the entire buffer instead of
        // panicking with an out-of-bounds index.
        assert_eq!(wide_array_to_string(&buf), "ABCD");
    }

    #[test]
    fn hardware_id_constants_match_inf_contract() {
        // The production INF (enterprise/virtual-display-driver/) MUST
        // advertise this exact hardware ID, otherwise SwDeviceCreate
        // reports no driver match. Kept deliberately different from
        // the PoC HW ID (`LcxlIddSampleDriver`, see
        // pocs/poc-indirect-display/driver/IddSampleDriver.inf) so the
        // two INFs can coexist on a dev box.
        assert_eq!(LCXL_IDD_HARDWARE_ID, "LcxlVirtualDisplay");
        assert_eq!(LCXL_IDD_INSTANCE_ID, "LcxlVirtualDisplay");
        assert_ne!(LCXL_IDD_HARDWARE_ID, "LcxlIddSampleDriver");
    }
}
