use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_utils::error::DeskErrorCode;

use crate::{
    error::CaptureError,
    model::{
        audio_capture::{AudioBuffer, WaveFormat},
        audio_encoder::{AudioEncoder, EncodedAudioBuffer},
    },
};
const SIZE_20MS: usize = 48000 * 2 / 1000 * 20;
pub struct OpusAudioEncoder {
    pub encoder: opusic_c::Encoder,
    pub buffer: Vec<u8>,
    /// Wave format
    pub wave_format: WaveFormat,
}

impl OpusAudioEncoder {
    pub fn new(settings: &DeskSettings, wave_format: WaveFormat) -> Result<Self, CaptureError> {
        let opus_settings = settings.opus_encoder.clone().unwrap_or_default();
        let channels = match opus_settings.channels {
            1 => opusic_c::Channels::Mono,
            2 => opusic_c::Channels::Stereo,
            _ => {
                return CaptureError::custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!("Unsupported number of channels: {}", opus_settings.channels),
                );
            }
        };

        let application = match opus_settings.application.as_str() {
            "Audio" => opusic_c::Application::Audio,
            "Voip" => opusic_c::Application::Voip,
            "LowDelay" => opusic_c::Application::LowDelay,
            _ => {
                return CaptureError::custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!(
                        "Unsupported Opus application: {}",
                        opus_settings.application
                    ),
                );
            }
        };

        let encoder = opusic_c::Encoder::new(channels, opusic_c::SampleRate::Hz48000, application)?;

        Ok(Self {
            encoder,
            buffer: vec![],
            wave_format,
        })
    }
}

impl AudioEncoder for OpusAudioEncoder {
    fn encode(
        &mut self,
        audio_buffer: &dyn AudioBuffer,
    ) -> Result<EncodedAudioBuffer, CaptureError> {
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

            let len = self
                .encoder
                .encode_float_to_slice(input_buffer, &mut output)?;
            log::trace!("encode_float_to_slice len={}", len);
            output[..len].to_vec()
        } else if self.wave_format.bits_per_sample == 16 {
            let input_buffer = unsafe {
                core::slice::from_raw_parts(self.buffer.as_ptr() as *const u16, SIZE_20MS)
            };

            let mut output = [0; 4000];

            let len = self.encoder.encode_to_slice(input_buffer, &mut output)?;
            log::trace!("encode_to_vec len={}", len);

            output[..len].to_vec()
        } else {
            return CaptureError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
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

#[cfg(test)]
mod tests {
    use super::*;

    struct TestAudioBuffer {
        samples: Vec<f32>,
    }

    impl AudioBuffer for TestAudioBuffer {
        fn get_buffer_slice(&self) -> &[u8] {
            // The byte view borrows the same initialized f32 allocation and
            // covers exactly its initialized length.
            unsafe {
                std::slice::from_raw_parts(
                    self.samples.as_ptr().cast::<u8>(),
                    self.samples.len() * std::mem::size_of::<f32>(),
                )
            }
        }

        fn get_num_frames(&self) -> usize {
            self.samples.len() / 2
        }
    }

    fn one_channel_tone(active_channel: usize) -> TestAudioBuffer {
        const FRAMES: usize = 48_000 / 50;
        const FREQUENCY_HZ: f32 = 1_000.0;
        let mut samples = Vec::with_capacity(FRAMES * 2);
        for frame in 0..FRAMES {
            let phase = std::f32::consts::TAU * FREQUENCY_HZ * frame as f32 / 48_000.0;
            let sample = phase.sin() * 0.5;
            samples.push(if active_channel == 0 { sample } else { 0.0 });
            samples.push(if active_channel == 1 { sample } else { 0.0 });
        }
        TestAudioBuffer { samples }
    }

    fn rms(samples: impl Iterator<Item = f32>) -> f32 {
        let (sum, count) = samples.fold((0.0, 0_usize), |(sum, count), sample| {
            (sum + sample * sample, count + 1)
        });
        let mean_square = sum / count as f32;
        mean_square.sqrt()
    }

    fn encode_and_decode(active_channel: usize) -> (Vec<u8>, Vec<f32>) {
        let settings = DeskSettings::default();
        let wave_format = WaveFormat {
            format_tag: 3,
            channels: 2,
            samples_per_sec: 48_000,
            avg_bytes_per_sec: 48_000 * 2 * 4,
            block_align: 8,
            bits_per_sample: 32,
        };
        let mut encoder = OpusAudioEncoder::new(&settings, wave_format).unwrap();
        let mut decoder =
            opusic_c::Decoder::new(opusic_c::Channels::Stereo, opusic_c::SampleRate::Hz48000)
                .unwrap();
        let input = one_channel_tone(active_channel);
        let mut last_packet = Vec::new();
        let mut last_decoded = Vec::new();

        for _ in 0..3 {
            let encoded = encoder.encode(&input).unwrap();
            assert!(!encoded.data.is_empty());
            let mut decoded = vec![0.0; 960 * 2];
            let frames = decoder
                .decode_float_to_slice(&encoded.data, &mut decoded, false)
                .unwrap();
            decoded.truncate(frames * 2);
            last_packet = encoded.data;
            last_decoded = decoded;
        }

        (last_packet, last_decoded)
    }

    #[test]
    fn production_encoder_preserves_left_and_right_channels() {
        for active_channel in 0..2 {
            let (packet, decoded) = encode_and_decode(active_channel);
            assert_ne!(packet[0] & 0x04, 0, "Opus TOC must signal stereo");

            let left_rms = rms(decoded.iter().step_by(2).copied());
            let right_rms = rms(decoded.iter().skip(1).step_by(2).copied());
            let (main_rms, crosstalk_rms) = if active_channel == 0 {
                (left_rms, right_rms)
            } else {
                (right_rms, left_rms)
            };

            assert!(
                main_rms > 0.01,
                "active channel must contain decoded energy"
            );
            assert!(
                main_rms > crosstalk_rms * 10.0,
                "active channel RMS {main_rms} must dominate crosstalk RMS {crosstalk_rms}"
            );
        }
    }
}
