use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::{
    error::CaptureError,
    model::audio_capture::{AudioBuffer, AudioCapture, AudioDeviceEnumerator, WaveFormat},
};
use desk_signal_facade::model::{
    audio_capture::{AudioDataFlow, AudioDevice},
    desk_settings::DeskSettings,
};
use desk_utils::error::DeskErrorCode;
use screencapturekit::prelude::*;

/// `kAudioFormatFlagIsNonInterleaved` from CoreAudio: set when each channel is
/// delivered in its own buffer (planar) rather than interleaved.
const AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;

/// Bytes per f32 PCM sample.
const F32_BYTES: usize = 4;

/// First-frame handshake timeout: the audio format is only known once the first
/// callback delivers a sample with a format description.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// Copyable snapshot of an error. `CaptureError` is not `Clone`, so shared state
/// stores the code + message and reconstructs the error on demand.
#[derive(Clone)]
struct ErrDesc {
    code: DeskErrorCode,
    message: String,
}

impl ErrDesc {
    fn from(err: &CaptureError) -> Self {
        Self {
            code: err.to_error_code(),
            message: err.to_string(),
        }
    }
}

/// State shared between the capture callbacks and the synchronous consumer.
struct AudioInner {
    /// Interleaved little-endian f32 PCM (LRLR...).
    queue: VecDeque<u8>,
    /// Resolved once the first sample's format description is read.
    format: Option<WaveFormat>,
    /// Whether the source delivers planar (non-interleaved) buffers.
    planar: bool,
    /// Sticky error. Once set, `get_buffer` keeps returning it so the upstream
    /// pipeline rebuilds the capture (it never clears on its own).
    error: Option<ErrDesc>,
}

struct AudioState {
    inner: Mutex<AudioInner>,
    cond: Condvar,
}

impl AudioState {
    fn new() -> Self {
        Self {
            inner: Mutex::new(AudioInner {
                queue: VecDeque::new(),
                format: None,
                planar: false,
                error: None,
            }),
            cond: Condvar::new(),
        }
    }

    fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.clear();
        inner.format = None;
        inner.planar = false;
        inner.error = None;
    }
}

pub struct MacScreencaptureKitAudioCapture {
    stream: Option<SCStream>,
    shared: Arc<AudioState>,
    started: bool,
    format: Option<WaveFormat>,
}

pub struct MacScreencaptureKitAudioBuffer {
    buffer: Vec<u8>,
    num_frames: usize,
}

impl AudioBuffer for MacScreencaptureKitAudioBuffer {
    fn get_buffer_slice(&self) -> &[u8] {
        &self.buffer
    }

    fn get_num_frames(&self) -> usize {
        self.num_frames
    }
}

/// Build a `WaveFormat` for little-endian float PCM with the given parameters.
fn build_wave_format(sample_rate: u32, channels: u16, bits: u16) -> WaveFormat {
    let block_align = channels * (bits / 8);
    WaveFormat {
        format_tag: 3, // WAVE_FORMAT_IEEE_FLOAT
        channels,
        samples_per_sec: sample_rate,
        avg_bytes_per_sec: sample_rate * block_align as u32,
        block_align,
        bits_per_sample: bits,
    }
}

/// Validate an audio format from its ASBD-derived parts and produce the
/// `WaveFormat` plus a planar flag. Format mismatches are unrecoverable
/// (`SYSTEM_ERROR`) — they are surfaced through `start()` and fail the pipeline
/// rather than triggering a retry that would hit the same format again.
fn derive_format_from_parts(
    is_pcm: bool,
    is_float: bool,
    is_big_endian: bool,
    sample_rate: Option<f64>,
    channels: Option<u32>,
    bits: Option<u32>,
    flags: u32,
) -> Result<(WaveFormat, bool), CaptureError> {
    if !is_pcm {
        return Err(CaptureError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "audio format is not LPCM",
        ));
    }
    if !is_float {
        return Err(CaptureError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "audio format is not float PCM",
        ));
    }
    if is_big_endian {
        return Err(CaptureError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "audio format is big-endian; expected little-endian",
        ));
    }
    let sample_rate = sample_rate.ok_or_else(|| {
        CaptureError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, "audio sample rate missing")
    })?;
    let channels = channels.ok_or_else(|| {
        CaptureError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, "audio channel count missing")
    })?;
    let bits = bits.ok_or_else(|| {
        CaptureError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "audio bits-per-channel missing",
        )
    })?;
    if bits != 32 {
        return Err(CaptureError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("audio float PCM expected 32 bits per channel, got {bits}"),
        ));
    }
    if channels == 0 {
        return Err(CaptureError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "audio has zero channels",
        ));
    }
    let planar = flags & AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED != 0;
    let format = build_wave_format(sample_rate as u32, channels as u16, bits as u16);
    Ok((format, planar))
}

/// Convert ScreenCaptureKit audio buffers into interleaved little-endian f32
/// PCM (LRLR...). `planar` indicates non-interleaved input (one buffer per
/// channel). Returns a recoverable error on inconsistent input so the upstream
/// pipeline rebuilds the capture.
fn interleave_f32(
    buffers: &[&[u8]],
    channels: usize,
    planar: bool,
) -> Result<Vec<u8>, CaptureError> {
    if !planar {
        // Already interleaved: concatenate the buffer bytes verbatim.
        let mut out = Vec::new();
        for b in buffers {
            out.extend_from_slice(b);
        }
        return Ok(out);
    }

    // Planar: one buffer per channel, interleave sample by sample.
    if buffers.len() != channels {
        return Err(CaptureError::new_custom_error(
            DeskErrorCode::ACTION_NEED_RETRY,
            &format!(
                "planar audio buffer count {} != channel count {channels}",
                buffers.len()
            ),
        ));
    }
    if channels == 0 {
        return Ok(Vec::new());
    }
    let frame_bytes = buffers[0].len();
    if buffers.iter().any(|b| b.len() != frame_bytes) {
        return Err(CaptureError::new_custom_error(
            DeskErrorCode::ACTION_NEED_RETRY,
            "planar audio channels have mismatched frame counts",
        ));
    }
    if !frame_bytes.is_multiple_of(F32_BYTES) {
        return Err(CaptureError::new_custom_error(
            DeskErrorCode::ACTION_NEED_RETRY,
            "planar audio buffer is not f32-aligned",
        ));
    }
    let num_frames = frame_bytes / F32_BYTES;
    let mut out = Vec::with_capacity(frame_bytes * channels);
    for frame in 0..num_frames {
        let start = frame * F32_BYTES;
        for ch in buffers.iter() {
            out.extend_from_slice(&ch[start..start + F32_BYTES]);
        }
    }
    Ok(out)
}

struct AudioReceiver {
    shared: Arc<AudioState>,
}

impl SCStreamOutputTrait for AudioReceiver {
    fn did_output_sample_buffer(&self, sample_buffer: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Audio {
            return;
        }

        let mut inner = self.shared.inner.lock().unwrap();
        if inner.error.is_some() {
            return;
        }

        // Resolve the format from the first sample's format description.
        if inner.format.is_none() {
            let Some(fd) = sample_buffer.format_description() else {
                inner.error = Some(ErrDesc {
                    code: DeskErrorCode::SYSTEM_ERROR,
                    message: "audio sample missing format description".to_string(),
                });
                self.shared.cond.notify_one();
                return;
            };
            match derive_format_from_parts(
                fd.is_pcm(),
                fd.audio_is_float(),
                fd.audio_is_big_endian(),
                fd.audio_sample_rate(),
                fd.audio_channel_count(),
                fd.audio_bits_per_channel(),
                fd.audio_format_flags().unwrap_or(0),
            ) {
                Ok((format, planar)) => {
                    inner.format = Some(format);
                    inner.planar = planar;
                    self.shared.cond.notify_one();
                }
                Err(e) => {
                    inner.error = Some(ErrDesc::from(&e));
                    self.shared.cond.notify_one();
                    return;
                }
            }
        }

        let channels = inner.format.map(|f| f.channels as usize).unwrap_or(0);
        let planar = inner.planar;

        let Some(list) = sample_buffer.audio_buffer_list() else {
            return;
        };
        let mut bufs: Vec<&[u8]> = Vec::with_capacity(list.num_buffers());
        for i in 0..list.num_buffers() {
            if let Some(buf) = list.get(i) {
                bufs.push(buf.data());
            }
        }

        match interleave_f32(&bufs, channels, planar) {
            Ok(bytes) => inner.queue.extend(bytes),
            Err(e) => {
                inner.error = Some(ErrDesc::from(&e));
                self.shared.cond.notify_one();
            }
        }
    }
}

struct AudioDelegate {
    shared: Arc<AudioState>,
}

impl SCStreamDelegateTrait for AudioDelegate {
    fn did_stop_with_error(&self, error: SCError) {
        // Stream stop is recoverable: record a retry error and wake any waiter.
        let mut inner = self.shared.inner.lock().unwrap();
        inner.error = Some(ErrDesc {
            code: DeskErrorCode::ACTION_NEED_RETRY,
            message: format!("audio stream stopped: {error}"),
        });
        self.shared.cond.notify_one();
    }
}

impl MacScreencaptureKitAudioCapture {
    pub fn new(_settings: &DeskSettings) -> Result<Self, CaptureError> {
        Ok(Self {
            stream: None,
            shared: Arc::new(AudioState::new()),
            started: false,
            format: None,
        })
    }

    /// Tear down a half-built stream and clear state on the calling thread, then
    /// return the error. Callbacks never stop the stream themselves (they hold
    /// no `SCStream` handle and a synchronous stop inside the output dispatch
    /// queue could deadlock); the rollback always happens here.
    fn rollback(&mut self, stream: Option<SCStream>, err: CaptureError) -> CaptureError {
        if let Some(stream) = stream {
            let _ = stream.stop_capture();
        }
        self.stream = None;
        self.started = false;
        self.format = None;
        self.shared.reset();
        err
    }
}

impl AudioCapture for MacScreencaptureKitAudioCapture {
    fn start(&mut self) -> Result<WaveFormat, CaptureError> {
        if self.started
            && let Some(format) = self.format
        {
            return Ok(format);
        }

        let content = SCShareableContent::get().map_err(|e| {
            CaptureError::new_custom_error(DeskErrorCode::PERMISSION_ERROR, &e.to_string())
        })?;
        let displays = content.displays();
        let display = displays.first().ok_or_else(|| {
            CaptureError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, "No display found")
        })?;

        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();

        let config = SCStreamConfiguration::new()
            .with_captures_audio(true)
            .with_sample_rate(48000)
            .with_channel_count(2)
            .with_excludes_current_process_audio(false);

        self.shared.reset();

        let delegate = AudioDelegate {
            shared: self.shared.clone(),
        };
        let mut stream = SCStream::new_with_delegate(&filter, &config, delegate);

        let receiver = AudioReceiver {
            shared: self.shared.clone(),
        };
        if stream
            .add_output_handler(receiver, SCStreamOutputType::Audio)
            .is_none()
        {
            return Err(self.rollback(
                Some(stream),
                CaptureError::new_custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    "ScreenCaptureKit add_output_handler failed",
                ),
            ));
        }

        if let Err(e) = stream.start_capture() {
            return Err(self.rollback(
                Some(stream),
                CaptureError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &e.to_string()),
            ));
        }

        // Wait for the first sample to resolve the format (or for an error).
        let mut inner = self.shared.inner.lock().unwrap();
        while inner.format.is_none() && inner.error.is_none() {
            let (guard, result) = self
                .shared
                .cond
                .wait_timeout(inner, FIRST_FRAME_TIMEOUT)
                .unwrap();
            inner = guard;
            if result.timed_out() && inner.format.is_none() && inner.error.is_none() {
                drop(inner);
                return Err(self.rollback(
                    Some(stream),
                    CaptureError::new_custom_error(
                        DeskErrorCode::ACTION_NEED_RETRY,
                        "audio first-frame timeout",
                    ),
                ));
            }
        }

        if let Some(err) = inner.error.take() {
            drop(inner);
            return Err(self.rollback(
                Some(stream),
                CaptureError::new_custom_error(err.code, &err.message),
            ));
        }

        let format = inner
            .format
            .expect("format resolved before leaving the wait loop");
        drop(inner);

        self.stream = Some(stream);
        self.started = true;
        self.format = Some(format);
        Ok(format)
    }

    fn get_buffer(&self) -> Result<Box<dyn AudioBuffer + Send + Sync>, CaptureError> {
        let mut inner = self.shared.inner.lock().unwrap();
        // Sticky error: keep returning it (do not clear) so the upstream
        // pipeline recreates the capture. A rebuild swaps in a fresh state.
        if let Some(err) = &inner.error {
            return Err(CaptureError::new_custom_error(err.code, &err.message));
        }

        let len = inner.queue.len();
        let mut vec = Vec::with_capacity(len);
        vec.extend(inner.queue.drain(..));
        drop(inner);

        let block_align = self.format.map(|f| f.block_align as usize).unwrap_or(0);
        let num_frames = if block_align == 0 {
            0
        } else {
            vec.len() / block_align
        };

        Ok(Box::new(MacScreencaptureKitAudioBuffer {
            buffer: vec,
            num_frames,
        }))
    }

    fn stop(&mut self) -> Result<(), CaptureError> {
        if let Some(stream) = self.stream.take() {
            stream.stop_capture().map_err(|e| {
                CaptureError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &e.to_string())
            })?;
        }
        self.started = false;
        self.format = None;
        self.shared.reset();
        Ok(())
    }
}

#[derive(Default)]
pub struct MacScreencaptureKitAudioDeviceEnumerator;

impl MacScreencaptureKitAudioDeviceEnumerator {
    pub fn new() -> Self {
        Self
    }
}

impl AudioDeviceEnumerator for MacScreencaptureKitAudioDeviceEnumerator {
    fn get_device_list(&self) -> Result<Vec<AudioDevice>, CaptureError> {
        // ScreenCaptureKit captures system audio; it does not enumerate input
        // devices like WASAPI, so expose a single synthetic "System Audio" one.
        Ok(vec![AudioDevice {
            id: "system_audio".to_string(),
            firendly_name: "System Audio".to_string(),
            data_flow: AudioDataFlow::Capture,
            default: true,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32_le(v: f32) -> [u8; 4] {
        v.to_le_bytes()
    }

    #[test]
    fn build_wave_format_stereo_f32() {
        let fmt = build_wave_format(48000, 2, 32);
        assert_eq!(fmt.format_tag, 3);
        assert_eq!(fmt.channels, 2);
        assert_eq!(fmt.samples_per_sec, 48000);
        assert_eq!(fmt.bits_per_sample, 32);
        assert_eq!(fmt.block_align, 8);
        assert_eq!(fmt.avg_bytes_per_sec, 48000 * 8);
    }

    #[test]
    fn derive_format_accepts_interleaved_float_le() {
        let (fmt, planar) =
            derive_format_from_parts(true, true, false, Some(48000.0), Some(2), Some(32), 0)
                .expect("valid format");
        assert!(!planar);
        assert_eq!(fmt.channels, 2);
        assert_eq!(fmt.samples_per_sec, 48000);
    }

    #[test]
    fn derive_format_detects_planar_flag() {
        let (_, planar) = derive_format_from_parts(
            true,
            true,
            false,
            Some(48000.0),
            Some(2),
            Some(32),
            AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED,
        )
        .expect("valid planar format");
        assert!(planar);
    }

    #[test]
    fn derive_format_rejects_non_float_and_big_endian() {
        let not_float =
            derive_format_from_parts(true, false, false, Some(48000.0), Some(2), Some(32), 0);
        assert!(not_float.is_err());
        assert_eq!(
            not_float.unwrap_err().to_error_code(),
            DeskErrorCode::SYSTEM_ERROR
        );

        let big_endian =
            derive_format_from_parts(true, true, true, Some(48000.0), Some(2), Some(32), 0);
        assert_eq!(
            big_endian.unwrap_err().to_error_code(),
            DeskErrorCode::SYSTEM_ERROR
        );

        let not_pcm =
            derive_format_from_parts(false, true, false, Some(48000.0), Some(2), Some(32), 0);
        assert_eq!(
            not_pcm.unwrap_err().to_error_code(),
            DeskErrorCode::SYSTEM_ERROR
        );
    }

    #[test]
    fn interleave_planar_stereo_produces_lrlr() {
        // Left channel: [1.0, 3.0], right channel: [2.0, 4.0].
        let mut left = Vec::new();
        left.extend_from_slice(&f32_le(1.0));
        left.extend_from_slice(&f32_le(3.0));
        let mut right = Vec::new();
        right.extend_from_slice(&f32_le(2.0));
        right.extend_from_slice(&f32_le(4.0));

        let out = interleave_f32(&[&left, &right], 2, true).expect("interleave ok");
        let samples = crate::model::audio_capture::align_slice_byte::<f32>(&out);
        assert_eq!(samples, &[1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn interleave_interleaved_single_buffer_is_copied() {
        let mut data = Vec::new();
        for v in [1.0f32, 2.0, 3.0, 4.0] {
            data.extend_from_slice(&f32_le(v));
        }
        let out = interleave_f32(&[&data], 2, false).expect("copy ok");
        assert_eq!(out, data);
    }

    #[test]
    fn interleave_planar_rejects_mismatched_frame_counts() {
        let left = vec![0u8; 8]; // 2 frames
        let right = vec![0u8; 4]; // 1 frame
        let err = interleave_f32(&[&left, &right], 2, true).unwrap_err();
        assert_eq!(err.to_error_code(), DeskErrorCode::ACTION_NEED_RETRY);
    }

    #[test]
    fn interleave_planar_rejects_wrong_buffer_count() {
        let left = vec![0u8; 8];
        let err = interleave_f32(&[&left], 2, true).unwrap_err();
        assert_eq!(err.to_error_code(), DeskErrorCode::ACTION_NEED_RETRY);
    }

    #[test]
    fn get_buffer_drains_queue_and_counts_frames() {
        let capture = MacScreencaptureKitAudioCapture {
            stream: None,
            shared: Arc::new(AudioState::new()),
            started: true,
            format: Some(build_wave_format(48000, 2, 32)),
        };
        // Two stereo frames = 2 * 8 bytes.
        {
            let mut inner = capture.shared.inner.lock().unwrap();
            inner.queue.extend(vec![0u8; 16]);
        }
        let buffer = capture.get_buffer().expect("buffer");
        assert_eq!(buffer.get_num_frames(), 2);
        assert_eq!(buffer.get_buffer_slice().len(), 16);
        // Queue is drained after read.
        assert!(capture.shared.inner.lock().unwrap().queue.is_empty());
    }

    #[test]
    fn get_buffer_returns_sticky_retry_error() {
        let capture = MacScreencaptureKitAudioCapture {
            stream: None,
            shared: Arc::new(AudioState::new()),
            started: true,
            format: Some(build_wave_format(48000, 2, 32)),
        };
        {
            let mut inner = capture.shared.inner.lock().unwrap();
            inner.error = Some(ErrDesc {
                code: DeskErrorCode::ACTION_NEED_RETRY,
                message: "stream stopped".to_string(),
            });
        }
        let err1 = capture
            .get_buffer()
            .err()
            .expect("first call returns error");
        assert_eq!(err1.to_error_code(), DeskErrorCode::ACTION_NEED_RETRY);
        // Sticky: a second call still returns the error (not cleared).
        let err2 = capture
            .get_buffer()
            .err()
            .expect("second call still returns error");
        assert_eq!(err2.to_error_code(), DeskErrorCode::ACTION_NEED_RETRY);
    }
}
