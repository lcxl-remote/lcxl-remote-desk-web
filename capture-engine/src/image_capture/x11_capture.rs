use std::ptr;

use desk_signal_facade::model::{
    desk_settings::DeskSettings,
    image_capture::{DisplayInfo, DisplayRect},
};
use libc::{IPC_CREAT, IPC_PRIVATE, IPC_RMID, SHM_RDONLY, shmat, shmctl, shmdt, shmget};
use x11rb::{
    connection::{Connection, RequestConnection},
    errors::ConnectionError,
    protocol::{
        randr::{self, ConnectionExt},
        shm::{self, ConnectionExt as _},
        xproto::{ConnectionExt as XprotoExt, ImageFormat},
    },
    rust_connection::RustConnection,
};

use desk_utils::error::DeskErrorCode;

use crate::{
    error::CaptureError,
    model::image_capture::{
        CaptureRequest, CaptureResult, ImageCapture, ImageCaptureType, ImageInfo,
        ImageOutputEnumerator, ImageType,
    },
};

/// Each X11 display is exposed under the synthetic name
/// `X11 Display {index}` so capture targets can be addressed by
/// device_name like the Windows backends. RandR-output naming is a
/// candidate v5 improvement; the index-based form keeps the wire
/// contract uniform and lets `DeskSettings.video_device_name` carry a
/// real value on Linux without any UI changes.
fn x11_device_name(index: usize) -> String {
    format!("X11 Display {}", index)
}

fn parse_x11_device_name(name: &str) -> Option<usize> {
    name.strip_prefix("X11 Display ")
        .and_then(|s| s.parse::<usize>().ok())
}

const PLANE_MASK: u32 = !1;

pub type CoordinateType = i16;
pub type ProportionType = u16;

#[derive(Debug, Copy, Clone)]
pub struct Display {
    top: CoordinateType,
    left: CoordinateType,
    width: ProportionType,
    height: ProportionType,
}

pub struct X11ImageCapture {
    index: usize,
    screen: usize,
    connection: RustConnection,
    displays: Vec<Display>,
    primary_display_index: usize,
    shm_addr: *const u8,
    shm_id: Option<i32>,
    seg: Option<u32>,
}

/// Workaround for *const not being Send + Sync
/// This is only works in single thread, so it is safe to use in this case.
unsafe impl Send for X11ImageCapture {}

unsafe impl Sync for X11ImageCapture {}

pub struct X11ImageInfo {
    pub data: Vec<u8>,
    pub width: u16,
    pub height: u16,
}

impl ImageInfo for X11ImageInfo {
    fn get_type(&self) -> ImageType {
        ImageType::BGRA
    }

    fn get_data(&self) -> &[u8] {
        &self.data
    }

    fn get_width(&self) -> u32 {
        self.width as u32
    }

    fn get_height(&self) -> u32 {
        self.height as u32
    }
}

pub struct X11ImageOutputEnumerator {}

impl X11ImageOutputEnumerator {
    pub fn new() -> Self {
        Self {}
    }
}

impl ImageOutputEnumerator for X11ImageOutputEnumerator {
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        let mut display_list = vec![];
        let (connection, screen) = x11rb::connect(None)?;
        let (_primary_display_index, displays) = get_displays(&connection, screen)?;
        for (idx, d) in displays.iter().enumerate() {
            log::info!("{:?}", d);
            display_list.push(DisplayInfo {
                device_name: x11_device_name(idx),
                display_device_name: None,
                desktop_coordinates: DisplayRect {
                    left: d.left as i32,
                    top: d.top as i32,
                    right: (d.left + d.width as i16) as i32,
                    bottom: (d.top + d.height as i16) as i32,
                },
                attached_to_desktop: true,
                rotation: 0,
                resolutions: vec![],
            });
        }
        Ok(display_list)
    }
}

/// X11 capture implementation for Linux systems.
/// see https://github.com/klarity-app/captis/blob/master/src/linux.rs
impl ImageCapture for X11ImageCapture {
    fn capture(&mut self, _request: CaptureRequest) -> Result<CaptureResult, CaptureError> {
        let image_info = match self.seg {
            Some(_) => self.capture_shm(self.index)?,
            None => self.capture_standard(self.index)?,
        };
        Ok(CaptureResult {
            image: image_info,
            cursor_update: None,
            content_changed: true,
            dirty_rects: None,
        })
    }

    fn get_capture_type(&self) -> ImageCaptureType {
        ImageCaptureType::X11
    }

    fn get_current_output(&self) -> Result<DisplayInfo, CaptureError> {
        let index = self.index;
        self.displays
            .get(self.index)
            .map(|d| DisplayInfo {
                device_name: x11_device_name(index),
                display_device_name: None,
                desktop_coordinates: DisplayRect {
                    left: d.left as i32,
                    top: d.top as i32,
                    right: (d.left + d.width as i16) as i32,
                    bottom: (d.top + d.height as i16) as i32,
                },
                attached_to_desktop: true,
                rotation: 0,
                resolutions: vec![],
            })
            .ok_or_else(|| {
                CaptureError::X11ConnectionError(ConnectionError::IoError(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Couldn't find primary display",
                )))
            })
    }
}

impl Drop for X11ImageCapture {
    fn drop(&mut self) {
        if let Some(seg) = self.seg {
            self.connection.shm_detach(seg).ok();
            unsafe {
                shmdt(self.shm_addr as _);
                shmctl(self.shm_id.unwrap(), IPC_RMID, ptr::null_mut());
            }
        }
    }
}

impl X11ImageCapture {
    pub fn new(settings: &DeskSettings) -> Result<Self, CaptureError> {
        log::debug!("Creating X11ImageCapture with settings: {:?}", settings);
        let (connection, screen) = x11rb::connect(None)?;

        if connection
            .extension_information(randr::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Err(CaptureError::X11ConnectionError(
                ConnectionError::UnsupportedExtension,
            ));
        }

        let (seg, shm_id, shm_addr) = if connection
            .extension_information(shm::X11_EXTENSION_NAME)?
            .is_some()
        {
            (None, None, ptr::null())
        } else {
            let screen = &connection.setup().roots[screen];

            match connection.generate_id() {
                Ok(seg) => unsafe {
                    let shm_id = shmget(
                        IPC_PRIVATE,
                        (screen.width_in_pixels as usize * screen.height_in_pixels as usize) * 4,
                        IPC_CREAT | 0o777,
                    );

                    if shm_id < 0 {
                        return Err(CaptureError::X11ConnectionError(ConnectionError::IoError(
                            std::io::Error::last_os_error(),
                        )));
                    }

                    let shm_addr = shmat(shm_id, ptr::null(), SHM_RDONLY);

                    if (shm_addr as isize) < 0 {
                        return Err(CaptureError::X11ConnectionError(ConnectionError::IoError(
                            std::io::Error::last_os_error(),
                        )));
                    }

                    connection.shm_attach(seg, shm_id as u32, false)?;

                    (Some(seg), Some(shm_id), shm_addr as *const u8)
                },
                Err(_) => (None, None, ptr::null()),
            }
        };

        let (primary_display_index, displays) = get_displays(&connection, screen)?;

        // Map `video_device_name` ("X11 Display N") back to its
        // positional index against the same `displays` vector the
        // enumerator returned. Empty / unknown / unparseable names are
        // hard errors so the worker surfaces a structured failure to
        // the frontend — there is no silent fallback to "first
        // display" (mirrors the WGC/DXGI/GDI policy).
        let requested = &settings.video_device_name;
        if requested.is_empty() {
            return CaptureError::custom_error(
                DeskErrorCode::INVALID_PARAMS,
                "video_device_name is empty: no X11 display has been selected. \
                 Open the desktop dialog in the browser and pick a display \
                 before starting media.",
            );
        }
        let index = match parse_x11_device_name(requested) {
            Some(idx) if idx < displays.len() => idx,
            _ => {
                let available = (0..displays.len())
                    .map(|i| format!("{:?}", x11_device_name(i)))
                    .collect::<Vec<_>>()
                    .join(", ");
                return CaptureError::custom_error(
                    DeskErrorCode::INVALID_PARAMS,
                    &format!(
                        "device_name {:?} not enumerated by X11; enumerated: [{}]",
                        requested, available
                    ),
                );
            }
        };
        Ok(X11ImageCapture {
            index,
            screen,
            displays,
            primary_display_index,
            connection,
            shm_addr,
            shm_id,
            seg,
        })
    }

    /// Captures the screen using standard protocols, which are a lot less inefficient.
    fn capture_standard(
        &self,
        index: usize,
    ) -> Result<Box<dyn ImageInfo + Send + Sync>, CaptureError> {
        let display = self.displays.get(index).ok_or_else(|| {
            ConnectionError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Couldn't find specified Display",
            ))
        })?;

        let screen = &self.connection.setup().roots[self.screen];

        let root = screen.root;

        let x11_image = self
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                root,
                display.left as i16,
                display.top as i16,
                display.width,
                display.height,
                PLANE_MASK,
            )?
            .reply_unchecked()?
            .ok_or(ConnectionError::UnknownError)?;

        Ok(Box::new(X11ImageInfo {
            data: x11_image.data,
            width: display.width,
            height: display.height,
        }))
    }

    /// Captures the screen using the XShm protocol and shared memory causing the program to run
    /// hella lot faster.
    fn capture_shm(&self, index: usize) -> Result<Box<dyn ImageInfo + Send + Sync>, CaptureError> {
        let display = self.displays.get(index).ok_or_else(|| {
            ConnectionError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Couldn't find specified Display",
            ))
        })?;

        let screen = &self.connection.setup().roots[self.screen];

        let root = screen.root;

        let reply = self
            .connection
            .shm_get_image(
                root,
                display.left as i16,
                display.top as i16,
                display.width,
                display.height,
                PLANE_MASK,
                ImageFormat::Z_PIXMAP.into(),
                unsafe { self.seg.unwrap_unchecked() },
                0,
            )?
            .reply_unchecked()?
            .ok_or(ConnectionError::UnknownError)?;

        let data: &[u8] =
            unsafe { std::slice::from_raw_parts(self.shm_addr as _, (reply.size * 4) as usize) };

        let data = data.to_vec();

        Ok(Box::new(X11ImageInfo {
            data,
            width: display.width,
            height: display.height,
        }))
    }
}

fn get_displays(
    connection: &RustConnection,
    screen: usize,
) -> Result<(usize, Vec<Display>), ConnectionError> {
    let screen = &connection.setup().roots[screen];
    let mut primary_display_index = 0;
    let mut displays: Vec<Display> = vec![];

    // Literally copied from https://github.com/BoboTiG/python-mss/blob/master/mss/linux.py
    let crtcs = match connection.randr_get_screen_resources_current(screen.root) {
        Ok(resources) => {
            resources
                .reply_unchecked()?
                .ok_or_else(|| {
                    ConnectionError::IoError(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "Couldn't get_screen_resources",
                    ))
                })?
                .crtcs
        }
        Err(_) => {
            connection
                .randr_get_screen_resources(screen.root)?
                .reply_unchecked()?
                .ok_or_else(|| {
                    ConnectionError::IoError(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "Couldn't get_screen_resources",
                    ))
                })?
                .crtcs
        }
    };

    for crtc in crtcs {
        if let Some(crtc_info) = connection.randr_get_crtc_info(crtc, 0)?.reply_unchecked()? {
            let display = Display {
                top: crtc_info.y.into(),
                left: crtc_info.x.into(),
                width: crtc_info.width.into(),
                height: crtc_info.height.into(),
            };
            if display.top == 0 && display.left == 0 {
                primary_display_index = displays.len();
            }
            displays.push(display);
        }
    }

    Ok((primary_display_index, displays))
}
