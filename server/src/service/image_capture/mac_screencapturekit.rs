use std::sync::{Arc, Condvar, Mutex};

use crate::{
    error::DeskError,
    model::image_capture::{
        CaptureRequest, CaptureResult, ImageCapture, ImageCaptureType, ImageInfo,
        ImageOutputEnumerator, ImageType,
    },
};
use desk_signal_facade::model::{
    desk_settings::DeskSettings,
    image_capture::{DisplayInfo, DisplayRect, Resolution},
};
use desk_utils::error::DeskErrorCode;
use screencapturekit::{
    cm_sample_buffer::CMSampleBuffer,
    sc_content_filter::{InitParams, SCContentFilter},
    sc_error_handler::StreamErrorHandler,
    sc_output_handler::{SCStreamOutputType, StreamOutput},
    sc_shareable_content::SCShareableContent,
    sc_stream::SCStream,
    sc_stream_configuration::SCStreamConfiguration,
};
use screencapturekit_sys::{as_ptr::AsPtr, cv_pixel_buffer_ref::CVPixelBufferRef};

// FFI declarations for CoreVideo functions missing from the crate wrappers
#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferGetWidth(pixel_buffer: *const CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: *const CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: *const CVPixelBufferRef) -> usize;
}

struct CaptureState {
    frame: Mutex<Option<SCImageInfo>>,
    cond: Condvar,
}

pub struct MacScreencaptureKitImageCapture {
    stream: Option<SCStream>,
    shared: Arc<CaptureState>,
    width: u32,
    height: u32,
    capture_type: ImageCaptureType,
    current_display: DisplayInfo,
}

struct FrameReceiver {
    shared: Arc<CaptureState>,
}

impl StreamOutput for FrameReceiver {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if let SCStreamOutputType::Screen = of_type {
            if let Some(pixel_buffer) = sample.pixel_buffer {
                // We need to access the underlying CVPixelBufferRef to get dimensions and stride
                // functionality is limited in wrapper, causing us to rely on private field or sys_ref
                // sample.sys_ref.get_image_buffer() returns ShareId<CVImageBufferRef> which is compatible with CVPixelBufferRef

                if let Some(image_buffer_ref) = sample.sys_ref.get_image_buffer() {
                    // Safety: image_buffer_ref is essentially CVPixelBufferRef
                    let raw_ptr = image_buffer_ref.as_ptr() as *const CVPixelBufferRef;

                    if pixel_buffer.lock() {
                        unsafe {
                            let width = CVPixelBufferGetWidth(raw_ptr) as u32;
                            let height = CVPixelBufferGetHeight(raw_ptr) as u32;
                            let bytes_per_row = CVPixelBufferGetBytesPerRow(raw_ptr) as usize;
                            let base_address = pixel_buffer.get_base_adress();

                            if !base_address.is_null() {
                                // Copy data
                                // Assuming BGRA (4 bytes per pixel)
                                // We need to copy row by row if stride != width * 4
                                // Or just copy tight buffer

                                let row_len = (width * 4) as usize;
                                let mut data = Vec::with_capacity((width * height * 4) as usize);

                                let src_slice = std::slice::from_raw_parts(
                                    base_address as *const u8,
                                    bytes_per_row * height as usize,
                                );

                                for y in 0..height {
                                    let src_offset = (y as usize) * bytes_per_row;
                                    let src_row = &src_slice[src_offset..src_offset + row_len];
                                    data.extend_from_slice(src_row);
                                }

                                let info = SCImageInfo {
                                    data,
                                    width,
                                    height,
                                };

                                let mut frame = self.shared.frame.lock().unwrap();
                                *frame = Some(info);
                                self.shared.cond.notify_one();
                            }
                        }
                        pixel_buffer.unlock();
                    }
                }
            }
        }
    }
}

struct ErrorHandler;
impl StreamErrorHandler for ErrorHandler {
    fn on_error(&self) {
        log::error!("ScreenCaptureKit stream error");
    }
}

impl MacScreencaptureKitImageCapture {
    pub fn new(_settings: &DeskSettings) -> Result<Self, DeskError> {
        let content = SCShareableContent::try_current().map_err(|e| {
            DeskError::new_custom_error(DeskErrorCode::PERMISSION_ERROR, e.as_str())
        })?;
        let displays = content.displays;

        // Default to main display or first display
        let display = displays.first().ok_or(DeskError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "No displays found",
        ))?;

        let width = display.width as u32;
        let height = display.height as u32;

        let display_info = DisplayInfo {
            device_name: display.display_id.to_string(), // Or \\.\DISPLAY1 style if needed
            display_device_name: Some(format!("Display {}", display.display_id)),
            desktop_coordinates: DisplayRect {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            },
            resolutions: vec![Resolution::new(width, height)],
            attached_to_desktop: true,
            rotation: 0,
        };

        Ok(Self {
            stream: None,
            shared: Arc::new(CaptureState {
                frame: Mutex::new(None),
                cond: Condvar::new(),
            }),
            width,
            height,
            capture_type: ImageCaptureType::SCKIT,
            current_display: display_info,
        })
    }
}

impl ImageCapture for MacScreencaptureKitImageCapture {
    fn capture(&mut self, _request: CaptureRequest) -> Result<CaptureResult, DeskError> {
        if self.stream.is_none() {
            let content = SCShareableContent::try_current().map_err(|e| {
                DeskError::new_custom_error(DeskErrorCode::PERMISSION_ERROR, e.as_str())
            })?;
            let display = content.displays.first().ok_or(DeskError::new_custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "No display found",
            ))?;

            let filter = SCContentFilter::new(InitParams::Display(display.clone()));

            let config = SCStreamConfiguration::from_size(
                display.width as u32,
                display.height as u32,
                false,
            );
            // We want BGRA usually
            // config.set_pixel_format(screencapturekit::sc_sys::os_types::geometry::kCVPixelFormatType_32BGRA); // If available

            let receiver = FrameReceiver {
                shared: self.shared.clone(),
            };

            let mut stream = SCStream::new(filter, config, ErrorHandler);
            stream.add_output(receiver, SCStreamOutputType::Screen);

            stream.start_capture().map_err(|e| {
                DeskError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, e.as_str())
            })?;

            self.stream = Some(stream);

            // Wait a bit for first frame?
            // std::thread::sleep(std::time::Duration::from_millis(100)); // Logic handled by condvar now
        }

        let mut frame_guard = self.shared.frame.lock().unwrap();
        if frame_guard.is_none() {
            // Wait for up to 3 seconds
            let (guard, result) = self
                .shared
                .cond
                .wait_timeout(frame_guard, std::time::Duration::from_secs(3))
                .unwrap();
            frame_guard = guard;
            if result.timed_out() {
                return Err(DeskError::new_custom_error(
                    DeskErrorCode::ACTION_NEED_RETRY,
                    "No frame available (timeout)",
                ));
            }
        }

        if let Some(info) = frame_guard.take() {
            Ok(CaptureResult {
                image: Box::new(info),
                cursor_update: None,
            })
        } else {
            // Spurious wakeup or empty after wait
            Err(DeskError::new_custom_error(
                DeskErrorCode::ACTION_NEED_RETRY,
                "No frame available",
            ))
        }
    }

    fn get_capture_type(&self) -> ImageCaptureType {
        self.capture_type
    }

    fn get_current_output(&self) -> Result<DisplayInfo, DeskError> {
        Ok(self.current_display.clone())
    }
}

#[derive(Clone)]
struct SCImageInfo {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

impl ImageInfo for SCImageInfo {
    fn get_type(&self) -> ImageType {
        ImageType::BGRA
    }

    fn get_data(&self) -> &[u8] {
        &self.data
    }

    fn get_width(&self) -> u32 {
        self.width
    }

    fn get_height(&self) -> u32 {
        self.height
    }
}

pub struct MacScreencaptureKitImageOutputEnumerator;

impl MacScreencaptureKitImageOutputEnumerator {
    pub fn new() -> Self {
        Self
    }
}

impl ImageOutputEnumerator for MacScreencaptureKitImageOutputEnumerator {
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, DeskError> {
        let content = SCShareableContent::try_current().map_err(|e| {
            DeskError::new_custom_error(DeskErrorCode::PERMISSION_ERROR, e.as_str())
        })?;
        let mut display_infos = Vec::new();

        for display in content.displays {
            display_infos.push(DisplayInfo {
                device_name: display.display_id.to_string(),
                display_device_name: Some(format!("Display {}", display.display_id)),
                desktop_coordinates: DisplayRect {
                    left: 0,
                    top: 0,
                    right: display.width as i32,
                    bottom: display.height as i32,
                },
                resolutions: vec![Resolution::new(display.width as u32, display.height as u32)],
                attached_to_desktop: true,
                rotation: 0,
            });
        }

        Ok(display_infos)
    }
}
