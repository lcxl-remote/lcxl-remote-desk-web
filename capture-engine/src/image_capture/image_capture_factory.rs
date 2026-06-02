use std::{collections::BTreeMap, str::FromStr};

use desk_signal_facade::model::{desk_settings::DeskSettings, image_capture::DisplayInfo};
#[cfg(target_os = "windows")]
use desk_utils::error::DeskErrorCode;
use strum::IntoEnumIterator;

#[cfg(target_os = "macos")]
use crate::image_capture::mac_screencapturekit::{
    MacScreencaptureKitImageCapture, MacScreencaptureKitImageOutputEnumerator,
};
#[cfg(target_os = "windows")]
use crate::image_capture::{
    dxgi_capture::{DxgiImageCapture, DxgiImageOutputEnumerator},
    gdi_capture::{GdiImageCapture, GdiImageOutputEnumerator},
    wgc_capture::{WgcImageCapture, WgcImageOutputEnumerator},
};
#[cfg(target_os = "linux")]
use crate::image_capture::{
    wayland_portal_capture::{WaylandPortalImageCapture, WaylandPortalImageOutputEnumerator},
    x11_capture::{X11ImageCapture, X11ImageOutputEnumerator},
};
use crate::{
    error::CaptureError,
    model::image_capture::{
        ImageCapture, ImageCaptureType, ImageCaptureTypeHelper, ImageOutputEnumerator,
    },
};

impl ImageCaptureTypeHelper for DeskSettings {
    /// Returns the appropriate EncoderType based on the settings.
    fn get_image_capture_type(&self) -> Result<ImageCaptureType, CaptureError> {
        if let Some(ref image_capture) = self.image_capture {
            let result = ImageCaptureType::from_str(image_capture);
            if let Ok(image_capture_type) = result {
                return Ok(image_capture_type);
            } else {
                log::error!(
                    "Failed to parse image capture type: {}, use default setting, error: {}",
                    image_capture,
                    result.err().unwrap()
                );
            }
        }

        Ok(ImageCaptureType::default())
    }
}

/// Returns true when `err` represents a structural unavailability of WGC
/// in the current process / desktop context — i.e. `IsSupported()`
/// returned `Ok(false)` or failed with a Windows error such as
/// `0x80070424` (`ERROR_SERVICE_NOT_FOUND`, the RuntimeBroker is not
/// running, typical for SYSTEM-token / Winlogon workers).
///
/// Only `WgcImageCapture::new` and `WgcImageOutputEnumerator::new` tag
/// their two `IsSupported` failure paths with `FEATURE_UNAVAILABLE`;
/// all other WGC failures (`CreateForMonitor`, `EnumOutputs`, D3D
/// device, post-init `IsSupported` flip, …) keep their original error
/// code and will not match here. This prevents the factory's
/// WGC→DXGI fallback from masking real WGC initialization failures.
#[cfg(target_os = "windows")]
fn is_wgc_unavailable_error(err: &CaptureError) -> bool {
    matches!(
        err,
        CaptureError::CustomError(e) if e.error_code == DeskErrorCode::FEATURE_UNAVAILABLE
    )
}

/// Create a video encoder based on the settings.
pub fn create_image_capture(
    desk_settings: &DeskSettings,
) -> Result<Box<dyn ImageCapture + Send>, CaptureError> {
    let image_capture_type = desk_settings.get_image_capture_type()?;
    let capture: Box<dyn ImageCapture + Send> = match image_capture_type {
        #[cfg(target_os = "windows")]
        ImageCaptureType::WGC => match WgcImageCapture::new(desk_settings) {
            Ok(c) => Box::new(c),
            // Structural WGC unavailability (typically Winlogon /
            // SYSTEM token, where the WGC RuntimeBroker service is not
            // running). Silently fall back to DXGI for this capture
            // instance so the video pipeline keeps producing frames
            // while the worker is bound to the secure desktop.
            // Fallback is per-instance and not persisted: when the
            // worker returns to the user desktop, the next subscribe
            // will try WGC again.
            Err(e) if is_wgc_unavailable_error(&e) => {
                log::warn!(
                    "[capture-factory] WGC unavailable ({}); falling back to DXGI for this capture instance",
                    e
                );
                let mut dxgi_settings = desk_settings.clone();
                dxgi_settings.image_capture =
                    Some(<&'static str>::from(ImageCaptureType::DXGI).to_string());
                Box::new(DxgiImageCapture::new(&dxgi_settings)?)
            }
            Err(e) => return Err(e),
        },
        #[cfg(target_os = "windows")]
        ImageCaptureType::DXGI => Box::new(DxgiImageCapture::new(desk_settings)?),
        #[cfg(target_os = "windows")]
        ImageCaptureType::GDI => Box::new(GdiImageCapture::new(desk_settings)?),
        #[cfg(target_os = "linux")]
        ImageCaptureType::X11 => Box::new(X11ImageCapture::new(desk_settings)?),
        #[cfg(target_os = "linux")]
        ImageCaptureType::WAYLANDPORTAL => Box::new(WaylandPortalImageCapture::new(desk_settings)?),
        #[cfg(target_os = "macos")]
        ImageCaptureType::SCKIT => Box::new(MacScreencaptureKitImageCapture::new(desk_settings)?),
    };
    // Log both requested and effective backend — the only reliable
    // signal that fallback engaged. When fallback fires the two values
    // differ (e.g. requested=WGC, effective=DXGI).
    log::info!(
        "Image capture factory: backend instance created, requested={:?}, effective={:?}",
        image_capture_type,
        capture.get_capture_type()
    );
    Ok(capture)
}

pub fn list_image_capture() -> BTreeMap<String, Vec<DisplayInfo>> {
    let mut result = BTreeMap::new();
    for x in ImageCaptureType::iter() {
        let name: String = Into::<&'static str>::into(x).to_string();
        match list_image_output(x) {
            Ok(output_list) => {
                result.insert(name, output_list);
            }
            Err(e) => {
                // FEATURE_UNAVAILABLE is an expected condition on
                // Winlogon / SYSTEM-token workers (WGC RuntimeBroker
                // missing). Demote to WARN so service logs don't fill
                // with ERROR entries on every worker restart. Real
                // problems (other backends failing) still go to ERROR.
                #[cfg(target_os = "windows")]
                let is_expected_unavailable =
                    e.to_error_code() == DeskErrorCode::FEATURE_UNAVAILABLE;
                #[cfg(not(target_os = "windows"))]
                let is_expected_unavailable = false;
                if is_expected_unavailable {
                    log::warn!(
                        "[capture-factory] backend {} unavailable in current context (skipped from enumeration): {}",
                        name,
                        e
                    );
                } else {
                    log::error!(
                        "Failed to get image output list for type: {}, error: {}",
                        name,
                        e
                    );
                }
            }
        }
    }
    result
}

pub async fn list_image_capture_async() -> BTreeMap<String, Vec<DisplayInfo>> {
    match tokio::task::spawn_blocking(list_image_capture).await {
        Ok(result) => result,
        Err(err) => {
            log::error!(
                "Failed to list image capture backends in blocking task: {}",
                err
            );
            BTreeMap::new()
        }
    }
}

pub fn list_image_output(
    image_capture_type: ImageCaptureType,
) -> Result<Vec<DisplayInfo>, CaptureError> {
    let capture: Box<dyn ImageOutputEnumerator + Send> = match image_capture_type {
        #[cfg(target_os = "windows")]
        ImageCaptureType::WGC => Box::new(WgcImageOutputEnumerator::new()?),
        #[cfg(target_os = "windows")]
        ImageCaptureType::DXGI => Box::new(DxgiImageOutputEnumerator::new()),
        #[cfg(target_os = "windows")]
        ImageCaptureType::GDI => Box::new(GdiImageOutputEnumerator::new()),
        #[cfg(target_os = "linux")]
        ImageCaptureType::X11 => Box::new(X11ImageOutputEnumerator::new()),
        #[cfg(target_os = "linux")]
        ImageCaptureType::WAYLANDPORTAL => Box::new(WaylandPortalImageOutputEnumerator::new()),
        #[cfg(target_os = "macos")]
        ImageCaptureType::SCKIT => Box::new(MacScreencaptureKitImageOutputEnumerator::new()),
    };
    let output_list = capture.get_output_list()?;

    Ok(output_list)
}

/// Decides whether enumerating `backend` failing with `err` should retry on a
/// different backend, mirroring `create_image_capture`'s WGC→DXGI fallback:
/// only WGC that is structurally unavailable (SYSTEM / secure-desktop worker)
/// falls back to DXGI. Returns the backend to enumerate instead, or `None` to
/// surface `err` unchanged — so other WGC failures are not masked, and non-WGC
/// backends never fall back. Pure so the branches are unit-testable.
#[cfg(target_os = "windows")]
fn fallback_image_output_backend(
    backend: ImageCaptureType,
    err: &CaptureError,
) -> Option<ImageCaptureType> {
    if matches!(backend, ImageCaptureType::WGC) && is_wgc_unavailable_error(err) {
        Some(ImageCaptureType::DXGI)
    } else {
        None
    }
}

/// Enumerate the displays for the backend that `create_image_capture` would
/// actually use for `settings`, applying the same WGC→DXGI fallback so callers
/// validate a capture target against the list that will really be used (e.g.
/// on a SYSTEM / secure-desktop worker where WGC is unavailable and the
/// factory silently builds a DXGI capture instead).
pub fn list_effective_image_output(
    settings: &DeskSettings,
) -> Result<Vec<DisplayInfo>, CaptureError> {
    let backend = settings.get_image_capture_type()?;
    match list_image_output(backend) {
        Ok(list) => Ok(list),
        Err(e) => {
            #[cfg(target_os = "windows")]
            if let Some(fallback) = fallback_image_output_backend(backend, &e) {
                log::warn!(
                    "[capture-factory] {backend:?} enumeration unavailable ({e}); \
                     enumerating {fallback:?} displays for effective target resolution"
                );
                return list_image_output(fallback);
            }
            Err(e)
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn is_wgc_unavailable_error_recognizes_feature_unavailable() {
        let err = CaptureError::new_custom_error(
            DeskErrorCode::FEATURE_UNAVAILABLE,
            "WGC requires Windows 10 1903+",
        );
        assert!(is_wgc_unavailable_error(&err));
    }

    #[test]
    fn is_wgc_unavailable_error_rejects_other_codes() {
        for code in [
            DeskErrorCode::SYSTEM_ERROR,
            DeskErrorCode::WINDOWS_ERROR,
            DeskErrorCode::INVALID_PARAMS,
            DeskErrorCode::PERMISSION_ERROR,
            DeskErrorCode::INVALID_STATE,
        ] {
            let err = CaptureError::new_custom_error(code, "x");
            assert!(
                !is_wgc_unavailable_error(&err),
                "code {:?} must not be classified as WGC-unavailable",
                code
            );
        }
    }

    #[test]
    fn fallback_backend_wgc_unavailable_falls_back_to_dxgi() {
        let err = CaptureError::new_custom_error(
            DeskErrorCode::FEATURE_UNAVAILABLE,
            "WGC RuntimeBroker not running",
        );
        assert!(
            matches!(
                fallback_image_output_backend(ImageCaptureType::WGC, &err),
                Some(ImageCaptureType::DXGI)
            ),
            "WGC + FEATURE_UNAVAILABLE must fall back to DXGI"
        );
    }

    #[test]
    fn fallback_backend_wgc_other_error_is_not_masked() {
        for code in [
            DeskErrorCode::SYSTEM_ERROR,
            DeskErrorCode::WINDOWS_ERROR,
            DeskErrorCode::INVALID_PARAMS,
        ] {
            let err = CaptureError::new_custom_error(code, "x");
            assert!(
                fallback_image_output_backend(ImageCaptureType::WGC, &err).is_none(),
                "WGC + {:?} must not be masked by a DXGI fallback",
                code
            );
        }
    }

    #[test]
    fn fallback_backend_non_wgc_never_falls_back() {
        let err = CaptureError::new_custom_error(DeskErrorCode::FEATURE_UNAVAILABLE, "unavailable");
        // Even with the WGC-unavailable code, non-WGC backends must surface
        // their own error rather than retry on a different backend.
        assert!(fallback_image_output_backend(ImageCaptureType::DXGI, &err).is_none());
        assert!(fallback_image_output_backend(ImageCaptureType::GDI, &err).is_none());

        let other = CaptureError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, "x");
        assert!(fallback_image_output_backend(ImageCaptureType::GDI, &other).is_none());
    }
}
