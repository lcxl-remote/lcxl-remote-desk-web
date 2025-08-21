use std::{mem, ptr};

use libc::{IPC_CREAT, IPC_PRIVATE, SHM_RDONLY, shmat, shmget};
use x11rb::{
    connection::{Connection, RequestConnection},
    errors::ConnectionError,
    protocol::{
        randr::{self, ConnectionExt},
        shm::{self, ConnectionExt as _},
    },
    rust_connection::RustConnection,
};

use crate::{
    desk_error::DeskError,
    model::{
        image_capture::{DisplayInfo, ImageCapture, ImageCaptureType, ImageInfo},
        settings::DeskSettings,
    },
};

#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub(crate) struct Bgr {
    b: u8,
    g: u8,
    r: u8,
    _padding: u8,
}

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
    screen: usize,
    connection: RustConnection,
    displays: Vec<Display>,
    primary_display_index: usize,
    shm_addr: *const u8,
    shm_id: Option<i32>,
    seg: Option<u32>,
}

impl ImageCapture for X11ImageCapture {
    fn capture(&mut self, show_mouse: bool) -> Result<Box<dyn ImageInfo + Send + Sync>, DeskError> {
        todo!()
    }

    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, DeskError> {
        todo!()
    }

    fn get_capture_type(&self) -> ImageCaptureType {
        ImageCaptureType::X11
    }
}

impl X11ImageCapture {
    pub fn new(settings: &DeskSettings) -> Result<Self, DeskError> {
        log::debug!("Creating X11ImageCapture with settings: {:?}", settings);
        let (connection, screen) = x11rb::connect(None)?;

        if connection
            .extension_information(randr::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Err(DeskError::X11ConnectionError(
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
                        (screen.width_in_pixels as usize * screen.height_in_pixels as usize)
                            * mem::size_of::<Bgr>(),
                        IPC_CREAT | 0o777,
                    );

                    if shm_id < 0 {
                        return Err(DeskError::X11ConnectionError(ConnectionError::IoError(
                            std::io::Error::last_os_error(),
                        )));
                    }

                    let shm_addr = shmat(shm_id, ptr::null(), SHM_RDONLY);

                    if (shm_addr as isize) < 0 {
                        return Err(DeskError::X11ConnectionError(ConnectionError::IoError(
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
        Ok(X11ImageCapture {
            screen,
            displays,
            primary_display_index,
            connection,
            shm_addr,
            shm_id,
            seg,
        })
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
