use crate::{
    desk_error::DeskError,
    model::{
        audio_capture::{AudioBuffer, WaveFormat},
        audio_encoder::{AudioEncoder, EncodedAudioBuffer},
        common::ErrorCode,
        settings::DeskSettings,
    },
};
const SIZE_20MS: usize = 48000 * 2 / 1000 * 20;
pub struct OpusAudioEncoder {
    //pub encoder: opusic_c::Encoder,
    pub encoder: opus::Encoder,

    pub buffer: Vec<u8>,
    /// Wave format
    pub wave_format: WaveFormat,
}

/// Workaround for Arc not being Send + Sync
/// This is only works in single thread, so it is safe to use in this case.
unsafe impl Send for OpusAudioEncoder {}

unsafe impl Sync for OpusAudioEncoder {}

impl OpusAudioEncoder {
    pub fn new(settings: &DeskSettings, wave_format: WaveFormat) -> Result<Self, DeskError> {
        let opus_settings = settings.opus_encoder.clone().unwrap_or_default();
        let channels = match opus_settings.channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            _ => {
                return DeskError::custom_error(
                    ErrorCode::SYSTEM_ERROR,
                    format!("Unsupported number of channels: {}", opus_settings.channels),
                );
            }
        };
        let application = match opus_settings.application.as_str() {
            "Audio" => opus::Application::Audio,
            "Voip" => opus::Application::Voip,
            "LowDelay" => opus::Application::LowDelay,
            _ => {
                return DeskError::custom_error(
                    ErrorCode::SYSTEM_ERROR,
                    format!(
                        "Unsupported Opus application: {}",
                        opus_settings.application
                    ),
                );
            }
        };
        let encoder = opus::Encoder::new(opus_settings.sample_rate, channels, application)?;
        Ok(Self {
            encoder,
            buffer: vec![],
            wave_format,
        })
    }
}

impl AudioEncoder for OpusAudioEncoder {
    fn encode(&mut self, audio_buffer: &dyn AudioBuffer) -> Result<EncodedAudioBuffer, DeskError> {
        let frame_20ms_byte_len = SIZE_20MS * self.wave_format.bits_per_sample as usize / 8;

        self.buffer
            .extend_from_slice(audio_buffer.get_buffer_slice());

        let origin_num_frames = 0; // origin_num_frames

        let encoded_buffer = Vec::<u8>::new();
        // the internal buffer is not enough to hold a 20ms frame, read more data from the audio device
        if self.buffer.len() < frame_20ms_byte_len {
            return Ok(EncodedAudioBuffer {
                data: encoded_buffer,
                origin_num_frames,
            });
        }
        // u32 = u8*4

        let encoded_buffer = if self.wave_format.bits_per_sample == 32 {
            let input_buffer = unsafe {
                core::slice::from_raw_parts(self.buffer.as_ptr() as *const f32, SIZE_20MS)
            };

            let mut output = [0; 4000];

            let len = self.encoder.encode_float(input_buffer, &mut output)?;
            log::trace!("encode_float_to_slice len={}", len);
            output[..len].to_vec()
        } else if self.wave_format.bits_per_sample == 16 {
            let input_buffer = unsafe {
                core::slice::from_raw_parts(self.buffer.as_ptr() as *const i16, SIZE_20MS)
            };

            let mut output = [0; 4000];
            //let len = self.encoder.encode_to_vec(input_buffer, &mut output)?;
            let len = self.encoder.encode(input_buffer, &mut output)?;
            log::trace!("encode_to_vec len={}", len);

            output[..len].to_vec()
        } else {
            return DeskError::custom_error(
                ErrorCode::SYSTEM_ERROR,
                format!(
                    "Unsupport bit per sample: {}",
                    self.wave_format.bits_per_sample
                ),
            );
        };

        let removed: Vec<u8> = self.buffer.drain(0..frame_20ms_byte_len).collect();
        log::trace!("removed {} bytes from buffer", removed.len());
        let origin_num_frames = removed.len() / self.wave_format.bits_per_sample as usize;

        Ok(EncodedAudioBuffer {
            data: encoded_buffer,
            origin_num_frames,
        })
    }
}
