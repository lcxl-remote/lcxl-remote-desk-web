use std::str::FromStr;

use strum::IntoEnumIterator;

use crate::{
    desk_error::DeskError,
    model::{
        image_capture::{ImageCapture, ImageCaptureType, ImageCaptureTypeHelper},
        settings::DeskSettings,
    },
    service::image_capture::{dxgi_capture::DigxImageCapture, gdi_capture::GdiImageCapture},
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
) -> Result<Box<dyn ImageCapture + Send + Sync>, DeskError> {
    let capture: Box<dyn ImageCapture + Send + Sync> =
        match desk_settings.get_image_capture_type()? {
            ImageCaptureType::DIGX => Box::new(DigxImageCapture::new(desk_settings)?),
            ImageCaptureType::DGI => Box::new(GdiImageCapture::new(desk_settings)?),
        };
    Ok(capture)
}

pub fn image_capture_list() -> Vec<String> {
    ImageCaptureType::iter()
        .map(|x| Into::<&'static str>::into(x).to_string())
        .collect()
}
