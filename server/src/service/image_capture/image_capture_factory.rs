use std::{collections::BTreeMap, str::FromStr};

use desk_signal_facade::model::{desk_settings::DeskSettings, image_capture::DisplayInfo};
use strum::IntoEnumIterator;

#[cfg(target_os = "macos")]
use crate::service::image_capture::mac_screencapturekit::{
    MacScreencaptureKitImageCapture, MacScreencaptureKitImageOutputEnumerator,
};
#[cfg(target_os = "windows")]
use crate::service::image_capture::{
    dxgi_capture::{DigxImageCapture, DigxImageOutputEnumerator},
    gdi_capture::{GdiImageCapture, GdiImageOutputEnumerator},
};
#[cfg(target_os = "linux")]
use crate::service::image_capture::{
    wayland_portal_capture::{WaylandPortalImageCapture, WaylandPortalImageOutputEnumerator},
    x11_capture::{X11ImageCapture, X11ImageOutputEnumerator},
};
use crate::{
    error::DeskError,
    model::image_capture::{
        ImageCapture, ImageCaptureType, ImageCaptureTypeHelper, ImageOutputEnumerator,
    },
};

impl ImageCaptureTypeHelper for DeskSettings {
    /// Returns the appropriate EncoderType based on the settings.
    fn get_image_capture_type(&self) -> Result<ImageCaptureType, DeskError> {
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

/// Create a video encoder based on the settings.
pub fn create_image_capture(
    desk_settings: &DeskSettings,
) -> Result<Box<dyn ImageCapture + Send>, DeskError> {
    let image_capture_type = desk_settings.get_image_capture_type()?;
    let capture: Box<dyn ImageCapture + Send> = match image_capture_type {
        #[cfg(target_os = "windows")]
        ImageCaptureType::DIGX => Box::new(DigxImageCapture::new(desk_settings)?),
        #[cfg(target_os = "windows")]
        ImageCaptureType::DGI => Box::new(GdiImageCapture::new(desk_settings)?),
        #[cfg(target_os = "linux")]
        ImageCaptureType::X11 => Box::new(X11ImageCapture::new(desk_settings)?),
        #[cfg(target_os = "linux")]
        ImageCaptureType::WAYLANDPORTAL => Box::new(WaylandPortalImageCapture::new(desk_settings)?),
        #[cfg(target_os = "macos")]
        ImageCaptureType::SCKIT => Box::new(MacScreencaptureKitImageCapture::new(desk_settings)?),
    };
    log::info!(
        "Image capture factory: backend instance created, image_capture_type={:?}",
        image_capture_type
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
                log::error!(
                    "Failed to get image output list for type: {}, error: {}",
                    name,
                    e
                );
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
) -> Result<Vec<DisplayInfo>, DeskError> {
    let capture: Box<dyn ImageOutputEnumerator + Send> = match image_capture_type {
        #[cfg(target_os = "windows")]
        ImageCaptureType::DIGX => Box::new(DigxImageOutputEnumerator::new()),
        #[cfg(target_os = "windows")]
        ImageCaptureType::DGI => Box::new(GdiImageOutputEnumerator::new()),
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
