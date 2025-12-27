use std::{collections::HashMap, ffi::CStr, thread::JoinHandle, time::Duration};

use desk_signal_facade::model::{desk_settings::DeskSettings, image_capture::DisplayInfo};
use desk_utils::error::DeskErrorCode;
use pipewire::{
    context::Context,
    main_loop::MainLoop,
    properties::properties,
    spa::{
        param::{
            ParamType,
            format::{FormatProperties, MediaSubtype, MediaType},
            format_utils,
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::{self, Pod, serialize::PodSerializer},
        utils::{Fraction, Rectangle, SpaTypes},
    },
    types::ObjectType,
};
use serde::Deserialize;
use zbus::{
    blocking::Proxy,
    zvariant::{DeserializeDict, OwnedFd, OwnedObjectPath, Type},
};

use crate::{
    error::DeskError,
    model::image_capture::{ImageCapture, ImageInfo, ImageOutputEnumerator, ImageType},
    service::image_capture::pipewire_utils::{
        get_zbus_connection, get_zbus_portal_request, wait_zbus_response,
    },
};

#[allow(dead_code)]
#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
pub struct ScreenCastCreateSessionResponse {
    session_handle: String,
}

#[allow(dead_code)]
#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
pub struct ScreenCastStartStream {
    pub id: Option<String>,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
    pub source_type: Option<u32>,
    pub mapping_id: Option<String>,
}

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
pub struct ScreenCastStartResponse {
    pub streams: Option<Vec<(u32, ScreenCastStartStream)>>,
    #[allow(dead_code)]
    pub restore_token: Option<String>,
}

/// https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html
pub struct ScreenCast<'a> {
    proxy: Proxy<'a>,
}

impl ScreenCast<'_> {
    pub fn new() -> Result<Self, DeskError> {
        let conn = get_zbus_connection()?;
        let proxy = Proxy::new(
            conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.ScreenCast",
        )?;

        Ok(ScreenCast { proxy })
    }

    pub fn create_session(&self) -> Result<OwnedObjectPath, DeskError> {
        let conn = get_zbus_connection()?;

        let mut options = HashMap::new();

        let handle_token = rand::random::<u32>().to_string();
        let portal_request = get_zbus_portal_request(conn, &handle_token)?;

        options.insert("handle_token", zbus::zvariant::Value::from(&handle_token));

        let session_handle_token = rand::random::<u32>().to_string();
        options.insert(
            "session_handle_token",
            zbus::zvariant::Value::from(&session_handle_token),
        );

        self.proxy.call_method("CreateSession", &(options))?;

        let response: ScreenCastCreateSessionResponse = wait_zbus_response(&portal_request)?;

        let unique_name = conn
            .unique_name()
            .ok_or(DeskError::ZbusError(zbus::Error::Failure(
                "Failed to get unique name".to_owned(),
            )))?;
        let unique_identifier = unique_name.trim_start_matches(':').replace('.', "_");

        let session = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/session/{unique_identifier}/{session_handle_token}"
        ))?;

        if session.as_str() != response.session_handle {
            return Err(DeskError::ZbusError(zbus::Error::Failure(
                "Session handle mismatch".to_owned(),
            )));
        }

        Ok(session)
    }

    pub fn select_sources(&self, session: &OwnedObjectPath) -> Result<(), DeskError> {
        let conn = get_zbus_connection()?;

        let mut options = HashMap::new();

        let handle_token = rand::random::<u32>().to_string();
        let portal_request = get_zbus_portal_request(conn, &handle_token)?;

        options.insert("handle_token", zbus::zvariant::Value::from(handle_token));
        options.insert("types", zbus::zvariant::Value::from(1_u32));
        options.insert("multiple", zbus::zvariant::Value::from(false));

        self.proxy
            .call_method("SelectSources", &(session, options))?;

        portal_request.receive_signal("Response")?;

        Ok(())
    }

    pub fn start(&self, session: &OwnedObjectPath) -> Result<ScreenCastStartResponse, DeskError> {
        let conn = get_zbus_connection()?;

        let mut options = HashMap::new();

        let handle_token = rand::random::<u32>().to_string();
        let portal_request = get_zbus_portal_request(conn, &handle_token)?;

        options.insert("handle_token", zbus::zvariant::Value::from(&handle_token));

        self.proxy.call_method("Start", &(session, "", options))?;

        wait_zbus_response(&portal_request)
    }

    #[allow(dead_code)]
    pub fn open_pipe_wire_remote(&self, session: &OwnedObjectPath) -> Result<OwnedFd, DeskError> {
        let options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        let fd: OwnedFd = self.proxy.call("OpenPipeWireRemote", &(session, options))?;

        Ok(fd)
    }
}

#[derive(Debug)]
pub struct PipewireAudioBuffer {
    pub buffer: Vec<u8>,   // Raw image data
    pub num_frames: usize, // Number of frames
}

pub struct PipewireImageOutputEnumerator {}

impl PipewireImageOutputEnumerator {
    pub fn new() -> Self {
        Self {}
    }
}

impl ImageOutputEnumerator for PipewireImageOutputEnumerator {
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, DeskError> {
        // FIXME: implement real Pipewire display enumeration
        let display_info = DisplayInfo {
            device_name: "pipewire-display-default".to_string(),
            display_device_name: Some("pipewire-display-default".to_string()),
            desktop_coordinates: Default::default(),
            resolutions: vec![],
            attached_to_desktop: true,
            rotation: 0,
        };
        let display_info_list = vec![display_info];
        Ok(display_info_list)
    }
}

struct UserData {
    format: VideoInfoRaw,
    cursor_move: bool,
    captured_count: u64,
    main_sender: std::sync::mpsc::Sender<PipewireCallback>,
}

#[derive(Debug, Clone)]
pub struct PipewireImageInfo {
    pub image_type: ImageType,
    pub data: Vec<u8>,
    pub height: u32,
    pub width: u32,
}

impl ImageInfo for PipewireImageInfo {
    fn get_type(&self) -> ImageType {
        self.image_type
    }
    fn get_data(&self) -> &[u8] {
        self.data.as_slice()
    }

    fn get_width(&self) -> u32 {
        self.width
    }

    fn get_height(&self) -> u32 {
        self.height
    }
}

fn get_spa_definition() -> Result<pipewire::spa::pod::Object, DeskError> {
    let pod = pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::RGB,
            VideoFormat::RGBA,
            VideoFormat::RGBx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            // VideoFormat::YUY2,
            // VideoFormat::I420,
        ),
        pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 128,
                height: 128
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 4096,
                height: 4096
            }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 24, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    return Ok(pod);
}

fn pw_thread(
    main_sender: std::sync::mpsc::Sender<PipewireCallback>,
    pw_receiver: pipewire::channel::Receiver<PipewireCommand>,
) {
    let result = inner_pw_thread(main_sender, pw_receiver);
    if let Err(e) = result {
        log::error!("Pipewire thread error: {:?}", e);
    } else {
        log::info!("Pipewire thread exited normally");
    }
}

fn inner_pw_thread(
    main_sender: std::sync::mpsc::Sender<PipewireCallback>,
    pw_receiver: pipewire::channel::Receiver<PipewireCommand>,
) -> Result<(), DeskError> {
    let screen_cast = ScreenCast::new()?;
    let session = screen_cast.create_session()?;
    screen_cast.select_sources(&session)?;
    let response = screen_cast.start(&session)?;
    // 获取流节点ID
    let stream_id = response
        .streams
        .ok_or(DeskError::ZbusError(zbus::Error::Failure(
            "Stream ID not found".to_owned(),
        )))?
        .first()
        .ok_or(DeskError::ZbusError(zbus::Error::Failure(
            "Stream ID not found".to_owned(),
        )))?
        .0;

    pipewire::init();

    let main_loop = MainLoop::new(None)?;

    let context = Context::new(&main_loop)?;
    let core = context.connect(None)?;
    let data = UserData {
        format: Default::default(),
        cursor_move: false,
        main_sender,
        captured_count: 0,
    };

    let _listener = core
        .add_listener_local()
        .info(|i| log::debug!("VIDEO CORE:\n{i:#?}"))
        .error(|e, f, g, h| log::error!("{e},{f},{g},{h}"))
        .done(|d, _| log::debug!("DONE: {d}"))
        .register();
    let registry = core.get_registry()?;
    let _listener_reg = registry
        .add_listener_local()
        .global(|global| {
            log::info!(
                "object: id:{} type:{}/{}, props: {:?}",
                global.id,
                global.type_,
                global.version,
                global.props
            );
            match global.type_ {
                ObjectType::Node => {
                    log::info!("Found node: id: {}, version: {}", global.id, global.version);
                }
                ObjectType::Port => {
                    log::info!("Found port: id: {}, version: {}", global.id, global.version);
                }
                ObjectType::Client => {
                    log::info!(
                        "Found client: id: {}, version: {}",
                        global.id,
                        global.version
                    );
                }
                _ => {}
            }
            if let Some(props) = global.props {
                if props.get("media.class") == Some("Video/Sink") {
                    log::info!("Found image device: {:?}", props.get("device.name"));
                }
            }
        })
        .register();

    /* Create a simple stream, the simple stream manages the core and remote
     * objects for you if you don't need to deal with them.
     *
     * If you plan to autoconnect your stream, you need to provide at least
     * media, category and role properties.
     *
     * Pass your events and a user_data pointer as the last arguments. This
     * will inform you about the stream state. The most important event
     * you need to listen to is the process event where you need to produce
     * the data.
     */
    let mut props = properties! {
        *pipewire::keys::MEDIA_TYPE => "Video",
        *pipewire::keys::MEDIA_CATEGORY => "Capture",
        *pipewire::keys::MEDIA_ROLE => "Screen",
    };

    let stream = pipewire::stream::Stream::new(&core, "video-capture", props)?;

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, _user_data, old, new| {
            log::info!("Stream state changed from {:?} to {:?}", old, new);
        })
        .control_info(|_, _user_data, id: u32, control| {
            if let Some(control) = unsafe { control.as_ref() } {
                log::info!("Stream control info, id: {}, control: {:?}", id, control);
                let cstr = unsafe { CStr::from_ptr(control.name) };
                if let Ok(name) = cstr.to_str() {
                    log::info!("Stream control name: {}", name);
                }
            } else {
                log::info!("Stream control info, id: {}, control is NULL", id);
            }
        })
        .param_changed(|_, user_data, id: u32, param| {
            // NULL means to clear the format
            let Some(param) = param else {
                return;
            };

            let (media_type, media_subtype) = match format_utils::parse_format(param) {
                Ok(v) => v,
                Err(_) => return,
            };
            log::info!(
                "Stream param changed, id: {}, media type: {:?}, media sub type: {:?}",
                id,
                media_type,
                media_subtype
            );
            match id {
                x if x == pipewire::spa::param::ParamType::Format.as_raw() => {
                    // only accept raw image
                    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                        return;
                    }

                    // call a helper function to parse the format for us.
                    user_data
                        .format
                        .parse(param)
                        .expect("Failed to parse param changed to VideoInfoRaw");
                    user_data
                        .main_sender
                        .send(PipewireCallback::Format(user_data.format.clone()))
                        .expect("Failed to send image format to main thread");

                    log::info!(
                        "capturing video size :{:?} frame rate:{:?}",
                        user_data.format.size(),
                        user_data.format.framerate(),
                    );
                }
                _ => return,
            }
        })
        .process(|stream, user_data| match stream.dequeue_buffer() {
            None => log::error!("out of buffers"),
            Some(mut buffer) => {
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }

                let data = &mut datas[0];

                let size = user_data.format.size();
                if let Some(frame_data) = data.data() {
                    let pipewire_image_info = PipewireImageInfo {
                        image_type: match user_data.format.format() {
                            VideoFormat::RGB | VideoFormat::RGBx => ImageType::RGB,
                            VideoFormat::BGRA | VideoFormat::BGRx => ImageType::BGRA,
                            _ => {
                                log::error!("Unsupported format: {:?}", user_data.format.format());
                                return;
                            }
                        },
                        data: frame_data.to_vec(),
                        width: size.width,
                        height: size.height,
                    };

                    user_data
                        .main_sender
                        .send(PipewireCallback::ImageInfo(pipewire_image_info))
                        .expect("Failed to send video samples to main thread");
                }
            }
        })
        .register()?;

    let pw_obj = get_spa_definition()?;
    /* Make one parameter with the supported formats. The SPA_PARAM_EnumFormat
     * id means that this is a format enumeration (of 1 value).
     * We leave the channels and rate empty to accept the native graph
     * rate and channels. */
    let video_spa_values: Vec<u8> = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pipewire::spa::pod::Value::Object(pw_obj),
    )
    .unwrap()
    .0
    .into_inner();

    let mut video_params = [Pod::from_bytes(&video_spa_values).unwrap()];

    /* Now connect this stream. We ask that our process function is
     * called in a realtime thread. */
    stream.connect(
        pipewire::spa::utils::Direction::Input,
        Some(stream_id),
        pipewire::stream::StreamFlags::AUTOCONNECT
            | pipewire::stream::StreamFlags::MAP_BUFFERS
            | pipewire::stream::StreamFlags::RT_PROCESS,
        &mut video_params,
    )?;

    // When we receive a `Terminate` message, quit the main loop.
    let main_loop_for_pw = main_loop.clone();
    let _receiver = pw_receiver.attach(main_loop_for_pw.loop_(), {
        let mainloop = main_loop.clone();
        move |command| match command {
            PipewireCommand::Terminate => {
                log::warn!("PipewireLoop terminating");
                mainloop.quit()
            }
        }
    });

    main_loop.run();
    Ok(())
}

pub struct PipewireLoop {
    main_sender: std::sync::mpsc::Sender<PipewireCallback>,
    /// send terminate command to PipewireLoop when dropping
    pw_sender: pipewire::channel::Sender<PipewireCommand>,
    /// pipewire thread handle
    pw_thread: Option<JoinHandle<()>>,
}

#[derive(Debug, Copy, Clone)]
pub enum PipewireCommand {
    Terminate,
}

#[derive(Debug, Clone)]
pub enum PipewireCallback {
    ImageInfo(PipewireImageInfo),
    Format(VideoInfoRaw),
}

impl PipewireLoop {
    // https://gitlab.freedesktop.org/pipewire/pipewire-rs/-/blob/main/pipewire/examples/image-capture.rs?ref_type=heads
    pub fn new(
        desk_settings: &DeskSettings,
        main_sender: std::sync::mpsc::Sender<PipewireCallback>,
        pw_sender: pipewire::channel::Sender<PipewireCommand>,
        pw_receiver: pipewire::channel::Receiver<PipewireCommand>,
    ) -> Result<Self, DeskError> {
        let main_sender_for_pw = main_sender.clone();
        let pw_thread = Some(std::thread::spawn(move || {
            pw_thread(main_sender_for_pw, pw_receiver)
        }));

        Ok(Self {
            main_sender,
            pw_sender,
            pw_thread,
        })
    }
}

impl Drop for PipewireLoop {
    fn drop(&mut self) {
        // send terminate command to PipewireLoop when dropping
        let result = self.pw_sender.send(PipewireCommand::Terminate);
        if let Some(handle) = self.pw_thread.take() {
            handle.join().expect("Failed to join Pipewire thread");
        }
        log::warn!("PipewireLoop dropped, result: {:?}", result);
    }
}

pub struct PipewireImageCapture {
    pub desk_settings: DeskSettings,
    pub pipewire_loop: Option<PipewireLoop>,
    pub main_receiver: Option<std::sync::mpsc::Receiver<PipewireCallback>>,
    pub pw_sender: Option<pipewire::channel::Sender<PipewireCommand>>,
    pub format: Option<VideoInfoRaw>,
}

impl ImageCapture for PipewireImageCapture {
    fn capture(&mut self, show_mouse: bool) -> Result<Box<dyn ImageInfo + Send + Sync>, DeskError> {
        let receiver = self.main_receiver.as_ref().unwrap();
        let mut last_frame = None;
        for item in receiver.try_iter() {
            match item {
                PipewireCallback::ImageInfo(image_info) => {
                    log::info!("Captured image info: {:?}", image_info);
                    last_frame = Some(image_info);
                }
                _ => {
                    log::warn!("Unexpected callback: {:?}", item);
                }
            }
        }
        if let Some(image_info) = last_frame {
            return Ok(Box::new(image_info));
        } else {
            return DeskError::custom_error(
                DeskErrorCode::ACTION_NEED_RETRY,
                "No image frame captured".to_string(),
            );
        }
    }

    fn get_capture_type(&self) -> crate::model::image_capture::ImageCaptureType {
        todo!()
    }

    fn get_current_output(&self) -> Result<DisplayInfo, DeskError> {
        todo!()
    }
}

impl PipewireImageCapture {
    pub fn new(desk_settings: &DeskSettings) -> Result<Self, DeskError> {
        let (main_sender, main_receiver) = std::sync::mpsc::channel();
        let (pw_sender, pw_receiver) = pipewire::channel::channel();

        let pw_sender_clone = pw_sender.clone();

        let pipewire_loop =
            PipewireLoop::new(desk_settings, main_sender, pw_sender_clone, pw_receiver)?;

        let pipewire_callback = main_receiver.recv_timeout(Duration::from_secs(30))?;
        let audio_format;
        match pipewire_callback {
            PipewireCallback::Format(format) => {
                log::info!("Received image format: {:?}", format);
                audio_format = format;
            }
            _ => {
                log::error!("Expected format callback, got {:?}", pipewire_callback);
                return DeskError::custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    "Failed to get image format".to_string(),
                );
            }
        }

        Ok(Self {
            desk_settings: desk_settings.clone(),
            pipewire_loop: Some(pipewire_loop),
            main_receiver: Some(main_receiver),
            pw_sender: Some(pw_sender),
            format: Some(audio_format),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Once, time::Duration};

    use desk_utils::logs::init_logs;
    use log::LevelFilter;

    use super::*;
    static INIT: Once = Once::new();
    pub fn initialize() {
        INIT.call_once(|| {
            // initialization code here
            let _ = init_logs(LevelFilter::Debug);
        });
    }

    #[test]
    fn test_pipewire_loop() -> Result<(), DeskError> {
        initialize();
        let desk_settings = DeskSettings::default();
        let (main_sender, main_receiver) = std::sync::mpsc::channel();
        let (pw_sender, pw_receiver) = pipewire::channel::channel();

        let pw_sender_clone = pw_sender.clone();

        let pipewire_loop =
            PipewireLoop::new(&desk_settings, main_sender, pw_sender_clone, pw_receiver)?;
        for _ in 0..100 {
            match main_receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(callback) => match callback {
                    PipewireCallback::ImageInfo(data) => {
                        log::trace!("Received {:?} bytes of image data", data);
                    }
                    PipewireCallback::Format(format) => {
                        log::info!("Received image format: {:?}", format);
                    }
                },
                Err(e) => {
                    log::warn!("No image data received: {:?}", e);
                }
            }
        }

        Ok(())
    }

    #[test]
    fn test_pipewire_capture() -> Result<(), DeskError> {
        initialize();
        let desk_settings = DeskSettings::default();
        let mut pipewire_capture = PipewireImageCapture::new(&desk_settings)?;

        for _ in 0..10 {
            match pipewire_capture.capture(true) {
                Ok(image_info) => {
                    log::info!("Captured {:?} frames of image data", image_info.get_data())
                }
                Err(error) => log::error!("Failed to capture image: {:?}", error),
            }
        }
        log::info!("Stopped pipewire image capture");
        Ok(())
    }
}
