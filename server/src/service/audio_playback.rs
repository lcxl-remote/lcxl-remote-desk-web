use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{
    HeapRb,
    traits::{Consumer, Producer, Split},
};
use webrtc::track::track_remote::TrackRemote;

/// Number of samples to pre-buffer before starting playback (~60ms at 48kHz mono)
const PRE_BUFFER_SAMPLES: usize = 48000 * 60 / 1000;

/// Start audio playback for a remote audio track.
/// Spawns a background task that reads RTP packets, decodes Opus, and plays via cpal.
pub fn start_audio_playback<F>(track: Arc<TrackRemote>, on_error: F)
where
    F: FnOnce(String) + Send + 'static,
{
    std::thread::spawn(move || {
        if let Err(e) = run_audio_playback(track) {
            log::error!("Audio playback error: {}", e);
            on_error(e);
        }
    });
}

fn run_audio_playback(track: Arc<TrackRemote>) -> Result<(), String> {
    // Setup cpal output device
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("No audio output device found")?;

    log::info!(
        "Audio playback using device: {}",
        device.name().unwrap_or_default()
    );

    // Try to get a 48kHz mono f32 config (matches Opus decoder output)
    let desired_sample_rate = cpal::SampleRate(48000);
    let desired_channels = 1u16;

    let supported_configs = device
        .supported_output_configs()
        .map_err(|e| format!("Failed to get output configs: {}", e))?;

    let config = supported_configs
        .into_iter()
        .find(|c| {
            c.channels() == desired_channels
                && c.min_sample_rate() <= desired_sample_rate
                && c.max_sample_rate() >= desired_sample_rate
                && c.sample_format() == cpal::SampleFormat::F32
        })
        .map(|c| c.with_sample_rate(desired_sample_rate))
        .or_else(|| {
            // Fallback: use default config
            device.default_output_config().ok()
        })
        .ok_or("No suitable audio output config found")?;

    let sample_rate = config.sample_rate().0;
    let channels = config.channels();

    log::info!(
        "Audio output config: {}Hz, {} channels, {:?}",
        sample_rate,
        channels,
        config.sample_format()
    );

    // Create ring buffer for inter-thread audio transfer
    // Buffer enough for ~500ms of audio
    let buffer_size = sample_rate as usize * channels as usize / 2;
    let rb = HeapRb::<f32>::new(buffer_size);
    let (mut producer, mut consumer) = rb.split();

    let playback_started = Arc::new(AtomicBool::new(false));
    let playback_started_for_callback = playback_started.clone();

    // Build cpal output stream
    let stream = device
        .build_output_stream(
            &config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // Read available samples from ring buffer
                let read = consumer.pop_slice(data);
                // Fill remainder with silence
                for sample in &mut data[read..] {
                    *sample = 0.0;
                }
            },
            |err| {
                log::error!("cpal output stream error: {}", err);
            },
            None,
        )
        .map_err(|e| format!("Failed to build output stream: {}", e))?;

    stream
        .play()
        .map_err(|e| format!("Failed to start playback: {}", e))?;

    log::info!("Audio playback stream started");

    // Create Opus decoder
    let mut decoder =
        opusic_c::Decoder::new(opusic_c::Channels::Mono, opusic_c::SampleRate::Hz48000)
            .map_err(|e| format!("Failed to create Opus decoder: {:?}", e))?;

    // Decode buffer: 48kHz * 120ms max Opus frame = 5760 samples
    let mut decode_buf = vec![0.0f32; 5760];
    let mut pre_buffer = Vec::with_capacity(PRE_BUFFER_SAMPLES);

    // Read RTP packets in a blocking tokio runtime
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Failed to create tokio runtime: {}", e))?;

    rt.block_on(async {
        loop {
            match track.read_rtp().await {
                Ok((rtp_packet, _attributes)) => {
                    let payload = rtp_packet.payload;
                    if payload.is_empty() {
                        continue;
                    }

                    // Decode Opus payload to f32 PCM
                    match decoder.decode_float_to_slice(payload.as_ref(), &mut decode_buf, false) {
                        Ok(decoded_samples) => {
                            let pcm = &decode_buf[..decoded_samples];

                            // If output has more channels than decoder, duplicate mono to all channels
                            let samples: Vec<f32> = if channels > 1 {
                                pcm.iter()
                                    .flat_map(|&s| std::iter::repeat_n(s, channels as usize))
                                    .collect()
                            } else {
                                pcm.to_vec()
                            };

                            if !playback_started.load(Ordering::Relaxed) {
                                // Pre-buffer phase
                                pre_buffer.extend_from_slice(&samples);
                                if pre_buffer.len() >= PRE_BUFFER_SAMPLES * channels as usize {
                                    // Flush pre-buffer to ring buffer and start playback
                                    let written = producer.push_slice(&pre_buffer);
                                    if written < pre_buffer.len() {
                                        log::warn!(
                                            "Pre-buffer overflow, dropped {} samples",
                                            pre_buffer.len() - written
                                        );
                                    }
                                    pre_buffer.clear();
                                    playback_started_for_callback.store(true, Ordering::Relaxed);
                                    log::info!("Audio pre-buffer filled, playback starting");
                                }
                            } else {
                                // Normal playback: push to ring buffer
                                let written = producer.push_slice(&samples);
                                if written < samples.len() {
                                    log::trace!(
                                        "Ring buffer full, dropped {} samples",
                                        samples.len() - written
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("Opus decode error: {:?}", e);
                        }
                    }
                }
                Err(e) => {
                    log::info!("Audio track read ended: {}", e);
                    break;
                }
            }
        }
    });

    log::info!("Audio playback finished");
    Ok(())
}
