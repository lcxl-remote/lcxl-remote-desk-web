use std::time::Duration;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::model::audio_capture::SelectedAudioDevice;

/// H264 encoder settings
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(default)]
pub struct H264EncoderSettings {
    /// Bitrate in bps (bits per second), default is 10,000,000 bps (10 Mbps)
    pub bps: u32,
    /// Group of Pictures, default is 0, which means the encoder will decide the value.
    pub gop: u32,
}

impl Default for H264EncoderSettings {
    fn default() -> Self {
        Self {
            bps: 10_000_000,
            gop: 0,
        }
    }
}

/// VPX encoder settings
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(default)]
pub struct VpxEncoderSettings {
    /// Bitrate in bps (bits per second), default is 5,000,000 bps (5 Mbps)
    pub bps: u32,
}

impl Default for VpxEncoderSettings {
    fn default() -> Self {
        Self { bps: 5_000_000 }
    }
}

/// Opus encoder settings
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(default)]
pub struct OpusEncoderSettings {
    pub sample_rate: u32,
    pub channels: u32,
    pub application: String, // e.g., "audio", "voip", etc.
}

impl Default for OpusEncoderSettings {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            application: "Audio".to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, ToSchema)]
#[serde(default)]
pub struct HotkeySettings {
    /// HOT_KEY_MODIFIERS
    pub fsmodifiers: u32,
    pub vk: u32,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            // ALT + CTRL
            fsmodifiers: 3,
            vk: 'L' as u32,
        }
    }
}

/// Private screen settings
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, ToSchema)]
#[serde(default)]
pub struct PrivateScreenSettings {
    /// Optional image path for the private screen background
    pub image_path: Option<String>,
    /// Optional window style for the private screen window
    pub window_style: Option<u32>,
    /// Optional window extended style for the private screen window
    pub window_ex_style: Option<u32>,
    /// Optional hotkey settings for toggling the private screen
    pub hotkey: Option<HotkeySettings>,
}

impl Default for PrivateScreenSettings {
    fn default() -> Self {
        Self {
            image_path: None,
            window_style: None,
            window_ex_style: None,
            hotkey: None,
        }
    }
}

/// Desk settings
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, ToSchema)]
#[serde(default)]
pub struct DeskSettings {
    /// Enable D3D debug mode
    pub enable_d3d_debug: bool,
    /// Video device index
    pub video_device_index: u32,
    /// Video encode bitrate in bps (bits per second)
    pub video_encode_bps: u32,
    /// Enable adaptive web page resolution
    pub adaptive_web_page_resolution: bool,
    /// Video zoom ratio (e.g., 50 for 50% zoom)
    pub video_zoom_ratio: u32,
    /// Video frame rate (e.g., 30 fps)
    pub video_fps: u32,
    /// Enable mouse display on the screen
    pub show_mouse: bool,
    /// Selected image capture device
    pub image_capture: Option<String>,
    /// Selected audio capture device
    pub audio_capture: Option<String>,
    /// Video encoder name, None for auto detection
    pub video_encoder: Option<String>,
    /// Selected audio device
    pub audio_device: Option<SelectedAudioDevice>,
    /// Audio encoder name, None for auto detection
    pub audio_encoder: Option<String>,
    /// h264 encoder settings
    pub h264_encoder: Option<H264EncoderSettings>,
    /// VP8 encoder settings
    pub vp8_encoder: Option<VpxEncoderSettings>,
    /// VP9 encoder settings
    pub vp9_encoder: Option<VpxEncoderSettings>,
    /// opus encoder settings
    pub opus_encoder: Option<OpusEncoderSettings>,

    /// Private screen settings
    pub private_screen: PrivateScreenSettings,
}

impl DeskSettings {
    pub fn get_duration_by_video_fps(&self) -> Duration {
        let mut video_fps = self.video_fps;
        if video_fps <= 0 {
            video_fps = 1;
        }
        Duration::from_micros((1000_000 as f32 / video_fps as f32) as u64)
    }
}

impl Default for DeskSettings {
    fn default() -> Self {
        Self {
            enable_d3d_debug: false,
            video_device_index: 0,
            video_encode_bps: 10_000_000,
            adaptive_web_page_resolution: false,
            video_zoom_ratio: 100,
            video_fps: 60,
            show_mouse: true,
            image_capture: None,
            audio_capture: None,
            video_encoder: None,
            audio_device: None,
            audio_encoder: None,
            h264_encoder: None,
            vp8_encoder: None,
            vp9_encoder: None,
            opus_encoder: None,
            private_screen: PrivateScreenSettings::default(),
        }
    }
}
