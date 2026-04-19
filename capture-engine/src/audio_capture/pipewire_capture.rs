use std::{ffi::CStr, mem, thread::JoinHandle, time::Duration, u16};

use desk_signal_facade::model::{
    audio_capture::{AudioDataFlow, AudioDevice},
    desk_settings::DeskSettings,
};
use desk_utils::error::DeskErrorCode;
use pipewire::{
    context::Context,
    main_loop::MainLoop,
    properties::properties,
    spa::{
        param::{
            audio::AudioInfoRaw,
            format::{MediaSubtype, MediaType},
            format_utils,
        },
        pod::Pod,
    },
    types::ObjectType,
};

use crate::{
    error::CaptureError,
    model::audio_capture::{AudioBuffer, AudioCapture, AudioDeviceEnumerator, WaveFormat},
};

#[derive(Debug)]
pub struct PipewireAudioBuffer {
    pub buffer: Vec<u8>,   // Raw audio data
    pub num_frames: usize, // Number of frames
}

impl AudioBuffer for PipewireAudioBuffer {
    fn get_buffer_slice(&self) -> &[u8] {
        &self.buffer
    }

    fn get_num_frames(&self) -> usize {
        self.num_frames
    }
}

pub struct PipewireAudioDeviceEnumerator {}

impl PipewireAudioDeviceEnumerator {
    pub fn new() -> Self {
        Self {}
    }
}

impl AudioDeviceEnumerator for PipewireAudioDeviceEnumerator {
    fn get_device_list(&self) -> Result<Vec<AudioDevice>, CaptureError> {
        // FIXME list all audio devices
        let audio_device = AudioDevice {
            id: "pipewire-audio-default".to_string(),
            firendly_name: "pipewire-audio-default".to_string(),
            data_flow: AudioDataFlow::Capture,
            default: true,
        };
        let audio_device_list = vec![audio_device];

        Ok(audio_device_list)
    }
}

struct UserData {
    format: AudioInfoRaw,
    cursor_move: bool,
    captured_count: u64,
    main_sender: std::sync::mpsc::Sender<PipewireCallback>,
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
) -> Result<(), CaptureError> {
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
                if props.get("media.class") == Some("Audio/Device") {
                    log::info!("Found audio device: {:?}", props.get("device.name"));
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
        *pipewire::keys::MEDIA_TYPE => "Audio",
        *pipewire::keys::MEDIA_CATEGORY => "Capture",
        *pipewire::keys::MEDIA_ROLE => "Music",
    };
    //if you want to capture from the sink monitor ports
    props.insert(*pipewire::keys::STREAM_CAPTURE_SINK, "true");

    let stream = pipewire::stream::Stream::new(&core, "audio-capture", props)?;

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
                    // only accept raw audio
                    if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                        return;
                    }

                    // call a helper function to parse the format for us.
                    user_data
                        .format
                        .parse(param)
                        .expect("Failed to parse param changed to AudioInfoRaw");
                    user_data
                        .main_sender
                        .send(PipewireCallback::Format(user_data.format.clone()))
                        .expect("Failed to send audio format to main thread");

                    log::info!(
                        "capturing rate:{} channels:{}",
                        user_data.format.rate(),
                        user_data.format.channels()
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
                let n_channels = user_data.format.channels();
                let n_samples = data.chunk().size() / (mem::size_of::<f32>() as u32);

                if let Some(samples) = data.data() {
                    let end_index = n_samples as usize * mem::size_of::<f32>();
                    user_data
                        .main_sender
                        .send(PipewireCallback::Stream(samples[0..end_index].to_vec()))
                        .expect("Failed to send audio samples to main thread");
                    if user_data.cursor_move {
                        user_data.captured_count = 0;
                        print!("\x1B[{}A", n_channels + 1);
                    }
                    user_data.captured_count += 1;
                    println!(
                        "captured {} samples, total count: {}",
                        n_samples / n_channels,
                        user_data.captured_count
                    );
                    for c in 0..n_channels {
                        let mut max: f32 = 0.0;
                        for n in (c..n_samples).step_by(n_channels as usize) {
                            let start = n as usize * mem::size_of::<f32>();
                            let end = start + mem::size_of::<f32>();
                            let chan = &samples[start..end];
                            let f = f32::from_le_bytes(chan.try_into().unwrap());
                            max = max.max(f.abs());
                        }

                        let peak = ((max * 30.0) as usize).clamp(0, 39);

                        println!(
                            "channel {}: |{:>w1$}{:w2$}| peak:{}",
                            c,
                            "*",
                            "",
                            max,
                            w1 = peak + 1,
                            w2 = 40 - peak
                        );
                    }
                    user_data.cursor_move = true;
                }
            }
        })
        .register()?;

    /* Make one parameter with the supported formats. The SPA_PARAM_EnumFormat
     * id means that this is a format enumeration (of 1 value).
     * We leave the channels and rate empty to accept the native graph
     * rate and channels. */
    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(pipewire::spa::param::audio::AudioFormat::F32LE);
    let obj = pipewire::spa::pod::Object {
        type_: pipewire::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pipewire::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = pipewire::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pipewire::spa::pod::Value::Object(obj),
    )
    .unwrap()
    .0
    .into_inner();

    let mut params = [Pod::from_bytes(&values).unwrap()];

    /* Now connect this stream. We ask that our process function is
     * called in a realtime thread. */
    stream.connect(
        pipewire::spa::utils::Direction::Input,
        None,
        pipewire::stream::StreamFlags::AUTOCONNECT
            | pipewire::stream::StreamFlags::MAP_BUFFERS
            | pipewire::stream::StreamFlags::RT_PROCESS,
        &mut params,
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
    Stream(Vec<u8>),
    Format(AudioInfoRaw),
}

impl PipewireLoop {
    // https://gitlab.freedesktop.org/pipewire/pipewire-rs/-/blob/main/pipewire/examples/audio-capture.rs?ref_type=heads
    pub fn new(
        desk_settings: &DeskSettings,
        main_sender: std::sync::mpsc::Sender<PipewireCallback>,
        pw_sender: pipewire::channel::Sender<PipewireCommand>,
        pw_receiver: pipewire::channel::Receiver<PipewireCommand>,
    ) -> Result<Self, CaptureError> {
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

pub struct PipewireAudioCapture {
    pub desk_settings: DeskSettings,
    pub pipewire_loop: Option<PipewireLoop>,
    pub main_receiver: Option<std::sync::mpsc::Receiver<PipewireCallback>>,
    pub pw_sender: Option<pipewire::channel::Sender<PipewireCommand>>,
    pub format: Option<AudioInfoRaw>,
}

impl AudioCapture for PipewireAudioCapture {
    fn start(&mut self) -> Result<WaveFormat, CaptureError> {
        if self.pipewire_loop.is_some() {
            return CaptureError::custom_error(
                DeskErrorCode::INVALID_STATE,
                "PipewireAudioCapture already started",
            );
        }
        let (main_sender, main_receiver) = std::sync::mpsc::channel();
        let (pw_sender, pw_receiver) = pipewire::channel::channel();

        let pw_sender_clone = pw_sender.clone();

        let pipewire_loop = PipewireLoop::new(
            &self.desk_settings,
            main_sender,
            pw_sender_clone,
            pw_receiver,
        )?;

        let pipewire_callback = main_receiver.recv_timeout(Duration::from_secs(30))?;
        let audio_format;
        match pipewire_callback {
            PipewireCallback::Format(format) => {
                log::info!("Received audio format: {:?}", format);
                audio_format = format;
            }
            _ => {
                log::error!("Expected format callback, got {:?}", pipewire_callback);
                return CaptureError::custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    "Failed to get audio format",
                );
            }
        }
        let mut wave_format = WaveFormat::default();
        wave_format.channels = audio_format.channels() as u16;
        wave_format.samples_per_sec = audio_format.rate();
        wave_format.bits_per_sample = match audio_format.format() {
            pipewire::spa::param::audio::AudioFormat::F32LE => 32,
            pipewire::spa::param::audio::AudioFormat::S16LE => 16,
            pipewire::spa::param::audio::AudioFormat::S32LE => 32,
            pipewire::spa::param::audio::AudioFormat::U16LE => 16,
            pipewire::spa::param::audio::AudioFormat::U32LE => 32,
            _ => {
                log::error!("Unsupported audio format: {:?}", audio_format.format());
                u16::MAX
            }
        };
        if wave_format.bits_per_sample == u16::MAX {
            return CaptureError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Unsupported audio format",
            );
        }
        wave_format.block_align = (wave_format.channels * wave_format.bits_per_sample / 8) as u16;
        wave_format.avg_bytes_per_sec =
            wave_format.samples_per_sec * wave_format.block_align as u32;
        wave_format.format_tag = 1; // PCM

        self.format = Some(audio_format);
        self.pipewire_loop = Some(pipewire_loop);
        self.main_receiver = Some(main_receiver);
        self.pw_sender = Some(pw_sender);

        Ok(wave_format)
    }

    fn get_buffer(&self) -> Result<Box<dyn AudioBuffer + Send + Sync>, CaptureError> {
        let receiver = self.main_receiver.as_ref().unwrap();
        let mut pipewire_audio_buffer = PipewireAudioBuffer {
            buffer: vec![],
            num_frames: 0,
        };
        let format = self.format.as_ref().unwrap();
        for item in receiver.try_iter() {
            match item {
                PipewireCallback::Stream(data) => {
                    // FIXME wrong number of frames
                    pipewire_audio_buffer.num_frames +=
                        data.len() / ((4 * format.channels()) as usize);
                    pipewire_audio_buffer.buffer.extend(data);
                }
                _ => {
                    log::warn!("Unexpected callback: {:?}", item);
                }
            }
        }
        return Ok(Box::new(pipewire_audio_buffer));
    }

    fn stop(&mut self) -> Result<(), CaptureError> {
        self.pipewire_loop = None;
        self.format = None;
        self.main_receiver = None;
        self.pw_sender = None;
        Ok(())
    }
}

impl PipewireAudioCapture {
    pub fn new(desk_settings: &DeskSettings) -> Result<Self, CaptureError> {
        Ok(Self {
            desk_settings: desk_settings.clone(),
            pipewire_loop: None,
            main_receiver: None,
            pw_sender: None,
            format: None,
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
    fn test_pipewire_loop() -> Result<(), CaptureError> {
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
                    PipewireCallback::Stream(data) => {
                        log::trace!("Received {} bytes of audio data", data.len());
                    }
                    PipewireCallback::Format(format) => {
                        log::info!("Received audio format: {:?}", format);
                    }
                },
                Err(e) => {
                    log::warn!("No audio data received: {}", e);
                }
            }
        }

        Ok(())
    }

    #[test]
    fn test_pipewire_capture() -> Result<(), CaptureError> {
        initialize();
        let desk_settings = DeskSettings::default();
        let mut pipewire_capture = PipewireAudioCapture::new(&desk_settings)?;
        let wave_format = pipewire_capture.start()?;
        log::info!(
            "Started pipewire audio capture with format: {:?}",
            wave_format
        );
        for _ in 0..10 {
            let audio_buffer = pipewire_capture.get_buffer()?;
            log::info!(
                "Captured {} frames of audio data",
                audio_buffer.get_num_frames()
            );
            if audio_buffer.get_num_frames() == 0 {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        pipewire_capture.stop()?;
        log::info!("Stopped pipewire audio capture");
        Ok(())
    }
}
