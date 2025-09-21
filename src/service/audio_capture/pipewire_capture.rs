use std::{ffi::CString, mem, sync::Arc, thread::JoinHandle};

use pipewire::{
    context::Context,
    core::Core,
    main_loop::{self, MainLoop},
    properties::properties,
    spa::{
        param::{
            format::{MediaSubtype, MediaType},
            format_utils,
        },
        pod::Pod,
    },
};

use crate::{
    desk_error::DeskError,
    model::{
        audio_capture::{
            AudioBuffer, AudioCapture, AudioDataFlow, AudioDevice, AudioDeviceEnumerator,
            WaveFormat,
        },
        settings::DeskSettings,
    },
};

pub struct PipewireAudioDeviceEnumerator {}

impl PipewireAudioDeviceEnumerator {
    pub fn new() -> Self {
        Self {}
    }
}

impl AudioDeviceEnumerator for PipewireAudioDeviceEnumerator {
    fn get_device_list(&self) -> Result<Vec<AudioDevice>, DeskError> {
        let mut audio_device_list = vec![];

        Ok(audio_device_list)
    }
}

struct UserData {
    format: pipewire::spa::param::audio::AudioInfoRaw,
    cursor_move: bool,
}

fn pw_thread(
    main_sender: std::sync::mpsc::Sender<String>,
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
    main_sender: std::sync::mpsc::Sender<String>,
    pw_receiver: pipewire::channel::Receiver<PipewireCommand>,
) -> Result<(), DeskError> {
    let main_loop = MainLoop::new(None)?;

    let context = Context::new(&main_loop)?;
    let core = context.connect(None)?;
    let data = UserData {
        format: Default::default(),
        cursor_move: false,
    };

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
        .param_changed(|_, user_data, id, param| {
            // NULL means to clear the format
            let Some(param) = param else {
                return;
            };
            if id != pipewire::spa::param::ParamType::Format.as_raw() {
                return;
            }

            let (media_type, media_subtype) = match format_utils::parse_format(param) {
                Ok(v) => v,
                Err(_) => return,
            };

            // only accept raw audio
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }

            // call a helper function to parse the format for us.
            user_data
                .format
                .parse(param)
                .expect("Failed to parse param changed to AudioInfoRaw");

            println!(
                "capturing rate:{} channels:{}",
                user_data.format.rate(),
                user_data.format.channels()
            );
        })
        .process(|stream, user_data| match stream.dequeue_buffer() {
            None => println!("out of buffers"),
            Some(mut buffer) => {
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }

                let data = &mut datas[0];
                let n_channels = user_data.format.channels();
                let n_samples = data.chunk().size() / (mem::size_of::<f32>() as u32);

                if let Some(samples) = data.data() {
                    if user_data.cursor_move {
                        print!("\x1B[{}A", n_channels + 1);
                    }
                    println!("captured {} samples", n_samples / n_channels);
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
    let mut audio_info = pipewire::spa::param::audio::AudioInfoRaw::new();
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
    pub main_sender: std::sync::mpsc::Sender<String>,
    /// send terminate command to PipewireLoop when dropping
    pub pw_sender: pipewire::channel::Sender<PipewireCommand>,

    pub pw_thread: Option<JoinHandle<()>>,
}

#[derive(Debug, Copy, Clone)]
pub enum PipewireCommand {
    Terminate,
}

impl PipewireLoop {
    // https://gitlab.freedesktop.org/pipewire/pipewire-rs/-/blob/main/pipewire/examples/audio-capture.rs?ref_type=heads
    pub fn new(
        desk_settings: &DeskSettings,
        main_sender: std::sync::mpsc::Sender<String>,
        pw_sender: pipewire::channel::Sender<PipewireCommand>,
        pw_receiver: pipewire::channel::Receiver<PipewireCommand>,
    ) -> Result<Self, DeskError> {
        pipewire::init();

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

pub struct PipewireAudioCapture {}

impl AudioCapture for PipewireAudioCapture {
    fn start(&mut self) -> Result<WaveFormat, DeskError> {
        let wave_format = WaveFormat::default();
        Ok(wave_format)
    }

    fn get_buffer(&self) -> Result<Box<dyn AudioBuffer + Send + Sync>, DeskError> {
        todo!()
    }

    fn stop(&mut self) -> Result<(), DeskError> {
        Ok(())
    }
}

impl PipewireAudioCapture {
    // https://gitlab.freedesktop.org/pipewire/pipewire-rs/-/blob/main/pipewire/examples/audio-capture.rs?ref_type=heads
    pub fn new(desk_settings: &DeskSettings) -> Result<Self, DeskError> {
        pipewire::init();
        let main_loop = MainLoop::new(None)?;

        let context = Context::new(&main_loop)?;
        let core = context.connect(None)?;

        let data = UserData {
            format: Default::default(),
            cursor_move: false,
        };

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
            .param_changed(|_, user_data, id, param| {
                // NULL means to clear the format
                let Some(param) = param else {
                    return;
                };
                if id != pipewire::spa::param::ParamType::Format.as_raw() {
                    return;
                }

                let (media_type, media_subtype) = match format_utils::parse_format(param) {
                    Ok(v) => v,
                    Err(_) => return,
                };

                // only accept raw audio
                if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                    return;
                }

                // call a helper function to parse the format for us.
                user_data
                    .format
                    .parse(param)
                    .expect("Failed to parse param changed to AudioInfoRaw");

                println!(
                    "capturing rate:{} channels:{}",
                    user_data.format.rate(),
                    user_data.format.channels()
                );
            })
            .process(|stream, user_data| match stream.dequeue_buffer() {
                None => println!("out of buffers"),
                Some(mut buffer) => {
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        return;
                    }

                    let data = &mut datas[0];
                    let n_channels = user_data.format.channels();
                    let n_samples = data.chunk().size() / (mem::size_of::<f32>() as u32);

                    if let Some(samples) = data.data() {
                        if user_data.cursor_move {
                            print!("\x1B[{}A", n_channels + 1);
                        }
                        println!("captured {} samples", n_samples / n_channels);
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
        let mut audio_info = pipewire::spa::param::audio::AudioInfoRaw::new();
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

        Ok(Self {})
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use log::LevelFilter;

    use super::*;
    use crate::utils::logs::init_logs;
    static INIT: Once = Once::new();
    pub fn initialize() {
        INIT.call_once(|| {
            // initialization code here
            let _ = init_logs(LevelFilter::Debug);
        });
    }

    #[test]
    fn test_device_info() -> Result<(), DeskError> {
        initialize();

        Ok(())
    }
}
