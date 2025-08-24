use std::str::FromStr;

use strum::IntoEnumIterator;

#[cfg(target_os = "linux")]
use crate::service::image_capture::x11_capture::X11ImageCapture;
#[cfg(target_os = "windows")]
use crate::service::image_capture::{dxgi_capture::DigxImageCapture, gdi_capture::GdiImageCapture};
use crate::{
    desk_error::DeskError,
    model::{
        common::ErrorCode,
        image_capture::{ImageCapture, ImageCaptureType, ImageCaptureTypeHelper},
        settings::DeskSettings,
    },
};

impl ImageCaptureTypeHelper for DeskSettings {
    /// Returns the appropriate EncoderType based on the settings.
    fn get_image_capture_type(&self) -> Result<ImageCaptureType, DeskError> {
        if let Some(ref image_capture) = self.image_capture {
            let result = ImageCaptureType::from_str(image_capture);
            if result.is_ok() {
                return Ok(result.unwrap());
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
        _ => {
            return DeskError::custom_error(
                ErrorCode::SYSTEM_ERROR,
                format!("Unsupported capture type:{:?}", image_capture_type),
            );
        }
    };
    Ok(capture)
}

pub fn image_capture_list() -> Vec<String> {
    ImageCaptureType::iter()
        .map(|x| Into::<&'static str>::into(x).to_string())
        .collect()
}
