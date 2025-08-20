use crate::{
    desk_error::DeskError,
    model::{image_capture::ImageCapture, settings::DeskSettings},
};

pub struct X11ImageCapture {}

impl ImageCapture for X11ImageCapture {
    fn capture(
        &mut self,
        show_mouse: bool,
    ) -> Result<
        Box<dyn crate::model::image_capture::ImageInfo + Send + Sync>,
        crate::desk_error::DeskError,
    > {
        todo!()
    }

    fn get_output_list(
        &self,
    ) -> Result<Vec<crate::model::image_capture::DisplayInfo>, crate::desk_error::DeskError> {
        todo!()
    }

    fn get_capture_type(&self) -> crate::model::image_capture::ImageCaptureType {
        todo!()
    }
}

impl X11ImageCapture {
    pub fn new(settings: &DeskSettings) -> Result<Self, DeskError> {
        log::debug!("Creating X11ImageCapture with settings: {:?}", settings);
        Ok(X11ImageCapture {})
    }
}
