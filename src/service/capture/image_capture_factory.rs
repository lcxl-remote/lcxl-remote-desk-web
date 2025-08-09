use crate::{
    desk_error::DeskError,
    model::{
        capture::{ImageCapture, ImageCaptureType, ImageCaptureTypeHelper},
        common::ErrorCode,
        settings::DeskSettings,
    },
    service::capture::dxgi_capture::DigxImageCapture,
};

impl ImageCaptureTypeHelper for DeskSettings {
    /// Returns the appropriate EncoderType based on the settings.
    fn get_capture_type(&self) -> Result<ImageCaptureType, DeskError> {
        let image_capture = {
            if let Some(ref image_capture) = self.image_capture {
                image_capture.clone()
            } else {
                "digx".to_string()
            }
        };
        match image_capture.as_str() {
            "digx" => Ok(ImageCaptureType::DIGX),
            _ => DeskError::custom_error(
                ErrorCode::SYSTEM_ERROR,
                format!("unknown image capture: {}", image_capture),
            ),
        }
    }
}

/// Create a video encoder based on the settings.
pub fn create_image_capture(
    desk_settings: &DeskSettings,
) -> Result<Box<dyn ImageCapture + Send + Sync>, DeskError> {
    let capture = match desk_settings.get_capture_type()? {
        ImageCaptureType::DIGX => Box::new(DigxImageCapture::new(desk_settings)?),
    };
    Ok(capture)
}
