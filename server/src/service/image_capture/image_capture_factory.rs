use std::{collections::BTreeMap, str::FromStr};

use strum::IntoEnumIterator;

#[cfg(target_os = "windows")]
use crate::service::image_capture::{
    dxgi_capture::{DigxImageCapture, DigxImageOutputEnumerator},
    gdi_capture::{GdiImageCapture, GdiImageOutputEnumerator},
};
#[cfg(target_os = "linux")]
use crate::service::image_capture::{
    pipewire_capture::{PipewireImageCapture, PipewireImageOutputEnumerator},
    x11_capture::{X11ImageCapture, X11ImageOutputEnumerator},
};
use crate::{
    error::DeskError,
    model::{
        image_capture::{
            DisplayInfo, ImageCapture, ImageCaptureType, ImageCaptureTypeHelper,
            ImageOutputEnumerator,
        },
        settings::DeskSettings,
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
        ImageCaptureType::PIPEWIRE => Box::new(PipewireImageCapture::new(desk_settings)?),
    };
    Ok(capture)
}

pub fn list_image_capture() -> BTreeMap<String, Vec<DisplayInfo>> {
    ImageCaptureType::iter()
        .map(|x| {
            //output_list_result =  get_image_output_list(x);
            (
                Into::<&'static str>::into(x).to_string(),
                list_image_output(x),
            )
        })
        .filter(|item| {
            if let Err(e) = &item.1 {
                log::error!(
                    "Failed to get image output list for type: {}, error: {:?}",
                    item.0,
                    e
                );
            }
            item.1.is_ok()
        })
        .map(|item| (item.0, item.1.unwrap()))
        .collect()
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
        ImageCaptureType::PIPEWIRE => Box::new(PipewireImageOutputEnumerator::new()),
    };
    let output_list = capture.get_output_list()?;

    Ok(output_list)
}
