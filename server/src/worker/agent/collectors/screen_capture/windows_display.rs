//! Bounded same-display fallback for one-shot captures on a removed DXGI device.

use desk_capture_engine::{
    error::CaptureError,
    model::image_capture::{ImageCaptureType, ImageCaptureTypeHelper, ImageInfo},
};
use desk_signal_facade::model::desk_settings::DeskSettings;
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED;

pub(super) fn capture(
    settings: &DeskSettings,
) -> Result<Box<dyn ImageInfo + Send + Sync>, CaptureError> {
    with_device_fallback(settings, super::capture_display_once)
}

fn with_device_fallback<T>(
    settings: &DeskSettings,
    mut capture: impl FnMut(&DeskSettings) -> Result<T, CaptureError>,
) -> Result<T, CaptureError> {
    match capture(settings) {
        Err(error)
            if matches!(
                settings.get_image_capture_type(),
                Ok(ImageCaptureType::DXGI)
            ) && matches!(&error, CaptureError::WindowsResultError(_, error) if error.code() == DXGI_ERROR_DEVICE_REMOVED) =>
        {
            // Preserve the exact resolved target. GDI's factory rejects missing
            // outputs; neither this retry nor the factory selects another one.
            let mut fallback = settings.clone();
            fallback.image_capture = Some("GDI".into());
            log::warn!(
                "One-shot screenshot: DXGI device removed; retrying the same display with GDI"
            );
            capture(&fallback)
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::E_ACCESSDENIED;

    #[test]
    fn device_fallback_is_once_and_preserves_the_target_and_saved_settings() {
        let settings = DeskSettings {
            video_device_name: r"\\.\DISPLAY2".into(),
            ..Default::default()
        };
        let mut calls = 0;
        let result: Result<(), CaptureError> = with_device_fallback(&settings, |current| {
            calls += 1;
            assert_eq!(current.video_device_name, settings.video_device_name);
            assert_eq!(
                current.image_capture.as_deref(),
                if calls == 1 { None } else { Some("GDI") }
            );
            Err(windows::core::Error::from_hresult(DXGI_ERROR_DEVICE_REMOVED).into())
        });
        assert!(result.is_err());
        assert_eq!(calls, 2);
        assert!(settings.image_capture.is_none());
    }

    #[test]
    fn access_denied_and_non_dxgi_errors_do_not_fall_back() {
        for (backend, code) in [("DXGI", E_ACCESSDENIED), ("GDI", DXGI_ERROR_DEVICE_REMOVED)] {
            let settings = DeskSettings {
                image_capture: Some(backend.into()),
                ..Default::default()
            };
            let mut calls = 0;
            let result: Result<(), CaptureError> = with_device_fallback(&settings, |_| {
                calls += 1;
                Err(windows::core::Error::from_hresult(code).into())
            });
            assert!(result.is_err());
            assert_eq!(calls, 1);
        }
    }
}
