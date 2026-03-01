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
        composite::{self, ConnectionExt as CompositeExt},
        randr::{self, ConnectionExt},
        render::{self, ConnectionExt as RenderExt},
        shm::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as XprotoExt, ImageFormat},
    },
    rust_connection::RustConnection,
};

use crate::{
    error::DeskError,
    model::image_capture::{
        ImageCapture, ImageCaptureType, ImageInfo, ImageOutputEnumerator, ImageType,
    },
};

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
    // --- XComposite fields ---
    exclude_window_id: u32,
    composite_active: bool,
    composite_pixmap: u32,
    composite_picture: u32,
    render_formats: Option<render::QueryPictFormatsReply>,
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
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, DeskError> {
        let mut display_list = vec![];
        let (connection, screen) = x11rb::connect(None)?;
        let (primary_display_index, displays) = get_displays(&connection, screen)?;
        for d in &displays {
            log::info!("{:?}", d);
            display_list.push(DisplayInfo {
                device_name: "X11 Display".to_string(),
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
    fn capture(
        &mut self,
        _show_mouse: bool,
    ) -> Result<Box<dyn ImageInfo + Send + Sync>, DeskError> {
        if self.exclude_window_id != 0 && self.composite_active {
            return self.capture_composite(self.index);
        }

        let image_info = match self.seg {
            Some(_) => self.capture_shm(self.index)?,
            None => self.capture_standard(self.index)?,
        };
        Ok(image_info)
    }

    fn get_capture_type(&self) -> ImageCaptureType {
        ImageCaptureType::X11
    }

    fn set_exclude_window(&mut self, window_id: u32) {
        let old_id = self.exclude_window_id;
        self.exclude_window_id = window_id;

        if window_id != 0 && !self.composite_active {
            let screen = &self.connection.setup().roots[self.screen];
            let root = screen.root;
            if let Err(e) = self
                .connection
                .composite_redirect_subwindows(root, composite::Redirect::AUTOMATIC)
            {
                log::error!("Failed to composite_redirect_subwindows: {:?}", e);
                self.exclude_window_id = old_id;
                return;
            }
            let _ = self.connection.flush();
            self.composite_active = true;
            log::info!(
                "XComposite redirect activated for exclude_window={}",
                window_id
            );
        } else if window_id == 0 && self.composite_active {
            let screen = &self.connection.setup().roots[self.screen];
            let root = screen.root;
            let _ = self
                .connection
                .composite_unredirect_subwindows(root, composite::Redirect::AUTOMATIC);
            let _ = self.connection.flush();
            self.composite_active = false;

            self.cleanup_composite_resources();
            log::info!("XComposite redirect deactivated");
        }
    }

    fn get_current_output(&self) -> Result<DisplayInfo, DeskError> {
        self.displays
            .get(self.index)
            .map(|d| DisplayInfo {
                device_name: "X11 Display".to_string(),
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
                DeskError::X11ConnectionError(ConnectionError::IoError(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Couldn't find primary display",
                )))
            })
    }
}

impl Drop for X11ImageCapture {
    fn drop(&mut self) {
        if self.composite_active {
            let screen = &self.connection.setup().roots[self.screen];
            let root = screen.root;
            let _ = self
                .connection
                .composite_unredirect_subwindows(root, composite::Redirect::AUTOMATIC);
        }
        self.cleanup_composite_resources();

        if let Some(seg) = self.seg {
            self.connection.shm_detach(seg).ok();
            unsafe {
                shmdt(self.shm_addr as _);
                shmctl(self.shm_id.unwrap(), IPC_RMID, ptr::null_mut());
            }
        }
    }
}

fn wrap_x11_err<E: std::fmt::Display>(e: E) -> DeskError {
    DeskError::X11ConnectionError(x11rb::errors::ConnectionError::IoError(
        std::io::Error::new(std::io::ErrorKind::Other, e.to_string()),
    ))
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
                        (screen.width_in_pixels as usize * screen.height_in_pixels as usize) * 4,
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
            index: settings.video_device_index as usize,
            screen,
            displays,
            primary_display_index,
            connection,
            shm_addr,
            shm_id,
            seg,
            exclude_window_id: 0,
            composite_active: false,
            composite_pixmap: 0,
            composite_picture: 0,
            render_formats: None,
        })
    }

    fn cleanup_composite_resources(&mut self) {
        if self.composite_picture != 0 {
            let _ = self.connection.render_free_picture(self.composite_picture);
            self.composite_picture = 0;
        }
        if self.composite_pixmap != 0 {
            let _ = self.connection.free_pixmap(self.composite_pixmap);
            self.composite_pixmap = 0;
        }
        let _ = self.connection.flush();
    }

    fn get_pict_format(&self, depth: u8) -> Result<render::Pictformat, DeskError> {
        let formats = self.render_formats.as_ref().unwrap();
        for format in &formats.formats {
            if format.depth == depth {
                return Ok(format.id);
            }
        }
        DeskError::custom_error(
            crate::error::DeskErrorCode::SYSTEM_ERROR,
            &format!("No PictFormat found for depth {}", depth),
        )
    }

    fn capture_composite(
        &mut self,
        index: usize,
    ) -> Result<Box<dyn ImageInfo + Send + Sync>, DeskError> {
        let display = self.displays.get(index).cloned().ok_or_else(|| {
            ConnectionError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Couldn't find specified Display",
            ))
        })?;

        let screen = &self.connection.setup().roots[self.screen];
        let root = screen.root;
        let root_depth = screen.root_depth;

        if self.render_formats.is_none() {
            self.render_formats = Some(
                self.connection
                    .render_query_pict_formats()?
                    .reply()
                    .map_err(wrap_x11_err)?,
            );
        }

        if self.composite_pixmap == 0 {
            self.composite_pixmap = self.connection.generate_id().map_err(wrap_x11_err)?;
            self.connection.create_pixmap(
                root_depth,
                self.composite_pixmap,
                root,
                display.width,
                display.height,
            )?;

            self.composite_picture = self.connection.generate_id().map_err(wrap_x11_err)?;
            let format_id = self.get_pict_format(root_depth)?;
            self.connection.render_create_picture(
                self.composite_picture,
                self.composite_pixmap,
                format_id,
                &render::CreatePictureAux::new(),
            )?;
        }

        self.connection.render_fill_rectangles(
            render::PictOp::SRC,
            self.composite_picture,
            render::Color {
                red: 0,
                green: 0,
                blue: 0,
                alpha: 0xFFFF,
            },
            &[xproto::Rectangle {
                x: 0,
                y: 0,
                width: display.width,
                height: display.height,
            }],
        )?;

        let tree = self
            .connection
            .query_tree(root)?
            .reply()
            .map_err(wrap_x11_err)?;

        let mut cookies = Vec::new();
        for &child in &tree.children {
            if child != self.exclude_window_id {
                let attr_cookie = self.connection.get_window_attributes(child)?;
                let geom_cookie = self.connection.get_geometry(child)?;
                cookies.push((child, attr_cookie, geom_cookie));
            }
        }

        for (child, attr_cookie, geom_cookie) in cookies {
            if let (Ok(attr), Ok(geom)) = (attr_cookie.reply(), geom_cookie.reply()) {
                if attr.map_state != xproto::MapState::VIEWABLE {
                    continue;
                }

                let src_x = (display.left - geom.x).max(0) as i16;
                let src_y = (display.top - geom.y).max(0) as i16;
                let dst_x = (geom.x - display.left).max(0) as i16;
                let dst_y = (geom.y - display.top).max(0) as i16;

                let avail_w = (geom.width as i16 - src_x).max(0) as u16;
                let avail_h = (geom.height as i16 - src_y).max(0) as u16;
                let copy_w = avail_w.min((display.width as i16 - dst_x).max(0) as u16);
                let copy_h = avail_h.min((display.height as i16 - dst_y).max(0) as u16);

                if copy_w > 0 && copy_h > 0 {
                    if let Ok(pict_format) = self.get_pict_format(geom.depth) {
                        if let Ok(win_pixmap) = self.connection.generate_id() {
                            if self
                                .connection
                                .composite_name_window_pixmap(child, win_pixmap)
                                .is_ok()
                            {
                                if let Ok(win_picture) = self.connection.generate_id() {
                                    let _ = self.connection.render_create_picture(
                                        win_picture,
                                        win_pixmap,
                                        pict_format,
                                        &render::CreatePictureAux::new(),
                                    );
                                    let _ = self.connection.render_composite(
                                        render::PictOp::OVER,
                                        win_picture,
                                        0u32,
                                        self.composite_picture,
                                        src_x,
                                        src_y,
                                        0,
                                        0,
                                        dst_x,
                                        dst_y,
                                        copy_w,
                                        copy_h,
                                    );
                                    let _ = self.connection.render_free_picture(win_picture);
                                }
                                let _ = self.connection.free_pixmap(win_pixmap);
                            }
                        }
                    }
                }
            }
        }

        self.connection.flush()?;

        let image = self
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.composite_pixmap,
                0,
                0,
                display.width,
                display.height,
                PLANE_MASK,
            )?
            .reply()
            .map_err(wrap_x11_err)?;

        Ok(Box::new(X11ImageInfo {
            data: image.data,
            width: display.width,
            height: display.height,
        }))
    }

    /// Captures the screen using standard protocols, which are a lot less inefficient.
    fn capture_standard(
        &self,
        index: usize,
    ) -> Result<Box<dyn ImageInfo + Send + Sync>, DeskError> {
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
    fn capture_shm(&self, index: usize) -> Result<Box<dyn ImageInfo + Send + Sync>, DeskError> {
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
