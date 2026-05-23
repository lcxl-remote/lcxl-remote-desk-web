use std::time::Duration;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::model::{audio_capture::SelectedAudioDevice, image_capture::DisplayInfo};

/// X264 encoder settings
#[derive(
    Clone,
    Debug,
    PartialEq,
    Deserialize,
    Serialize,
    ToSchema,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
#[serde(default)]
pub struct X264EncoderSettings {
    /// Quality (CRF), 0-51, default is 22. Lower is better.
    pub quality: u32,
    /// Group of Pictures, default is 0, which means the encoder will decide the value.
    pub gop: u32,
}

impl Default for X264EncoderSettings {
    fn default() -> Self {
        Self {
            quality: 22,
            gop: 0,
        }
    }
}
/// H264 encoder settings
#[derive(
    Clone,
    Debug,
    PartialEq,
    Deserialize,
    Serialize,
    ToSchema,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
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
            bps: 4_000_000,
            gop: 0,
        }
    }
}

/// VPX encoder settings
#[derive(
    Clone,
    Debug,
    PartialEq,
    Deserialize,
    Serialize,
    ToSchema,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
#[serde(default)]
pub struct VpxEncoderSettings {
    /// Bitrate in bps (bits per second), default is 50,000,000 bps (50 Mbps)
    pub bps: u32,
    /// Quality (CQ Level), 0-63, default is 25. Lower is better.
    pub quality: u32,
}

impl Default for VpxEncoderSettings {
    fn default() -> Self {
        Self {
            bps: 50_000_000,
            quality: 25,
        }
    }
}

/// AV1 encoder settings (rav1e)
#[derive(
    Clone,
    Debug,
    PartialEq,
    Deserialize,
    Serialize,
    ToSchema,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
#[serde(default)]
pub struct Av1EncoderSettings {
    /// Quality (Quantizer), 0-255, default is 100. Lower is better quality.
    pub quality: u32,
    /// Speed preset, 0-10, default is 10 (fastest). Lower is better quality but slower.
    pub speed: u32,
}

impl Default for Av1EncoderSettings {
    fn default() -> Self {
        Self {
            quality: 100,
            speed: 10, // 远程桌面场景需要最快速度
        }
    }
}

/// Opus encoder settings
#[derive(
    Clone,
    Debug,
    PartialEq,
    Deserialize,
    Serialize,
    ToSchema,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
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

#[derive(
    Clone,
    Debug,
    Deserialize,
    Serialize,
    PartialEq,
    ToSchema,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
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
#[derive(
    Clone,
    Debug,
    Deserialize,
    Serialize,
    PartialEq,
    ToSchema,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
#[serde(default)]
#[derive(Default)]
pub struct PrivateScreenSettings {
    /// Optional image path for the private screen background
    pub image_path: Option<String>,
    /// Optional window style for the private screen window
    pub window_style: Option<u32>,
    /// Optional window extended style for the private screen window
    pub window_ex_style: Option<u32>,
    /// Optional hotkey settings for toggling the private screen
    pub hotkey: Option<HotkeySettings>,
    /// Whether private screen is enabled by default
    pub enabled: bool,
}

/// Desk settings
#[derive(
    Clone,
    Debug,
    Deserialize,
    Serialize,
    PartialEq,
    ToSchema,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
#[serde(default)]
pub struct DeskSettings {
    /// Enable D3D debug mode
    pub enable_d3d_debug: bool,
    /// GDI device name of the capture target (`\\.\DISPLAYn`).
    /// Empty string means "no display selected yet" — the browser
    /// must surface a chooser before starting media. Selection is by
    /// name (instead of enumeration index) because:
    /// 1. DXGI and WGC backends walk different enumerations
    ///    (`IDXGIAdapter::EnumOutputs` vs `EnumDisplayMonitors`), but
    ///    both expose the same GDI device name — so a name-based key
    ///    is the only stable cross-backend addressing.
    /// 2. Display hot-plug (attach / detach / IDD bring-up) reorders
    ///    the enumeration, so an index saved in settings would drift
    ///    onto the wrong monitor across reboots.
    pub video_device_name: String,
    /// Video encode quality, 0-63, lower is better. Default is 22.
    pub video_quality: u32,
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
    /// x264 encoder settings
    pub x264_encoder: Option<X264EncoderSettings>,
    /// h264 encoder settings
    pub h264_encoder: Option<H264EncoderSettings>,
    /// VP8 encoder settings
    pub vp8_encoder: Option<VpxEncoderSettings>,
    /// VP9 encoder settings
    pub vp9_encoder: Option<VpxEncoderSettings>,
    /// opus encoder settings
    pub opus_encoder: Option<OpusEncoderSettings>,
    /// AV1 encoder settings
    pub av1_encoder: Option<Av1EncoderSettings>,

    /// Private screen settings
    pub private_screen: PrivateScreenSettings,
    /// Display name
    pub display_name: Option<String>,
    /// Wayland control mode: portal/uinput/none
    pub wayland_control_mode: Option<String>,
    /// Whether the encoder may honour `ImageInfo::get_dirty_rects` to only
    /// re-convert changed regions of the BGRA frame into the persistent YUV
    /// buffer. Defaults to `true` (the optimisation is on). Setting it to
    /// `false` forces every captured frame through a full BGRA→YUV
    /// conversion — useful as a kill-switch when partial updates surface
    /// rendering artefacts (e.g. transient black bars on animation-heavy
    /// content).
    pub enable_dirty_rect: bool,
}

impl DeskSettings {
    pub fn get_duration_by_video_fps(&self) -> Duration {
        let mut video_fps = self.video_fps;
        if video_fps == 0 {
            video_fps = 1;
        }
        Duration::from_micros((1_000_000_f32 / video_fps as f32) as u64)
    }

    pub fn get_x264_encoder_settings(&self) -> X264EncoderSettings {
        if let Some(ref x264_encoder) = self.x264_encoder {
            return x264_encoder.clone();
        }
        // use video_quality to create a default x264 encoder settings
        X264EncoderSettings {
            quality: self.video_quality,
            ..Default::default()
        }
    }

    pub fn get_h264_encoder_settings(&self, display_info: &DisplayInfo) -> H264EncoderSettings {
        if let Some(ref h264_encoder) = self.h264_encoder {
            return h264_encoder.clone();
        }
        // Use video_quality to create a default h264 encoder settings
        let mut encoder_settings = H264EncoderSettings::default();

        // Calculate bps based on resolution and quality
        // video_quality: 0 (highest) to 63 (lowest)
        // BPP (Bits Per Pixel) range: 0.02 (lowest) to 0.20 (highest)
        // For 1080p, 60fps:
        // Quality 0: 1920 * 1080 * 60 * 0.20 = 24.8 Mbps
        // Quality 63: 1920 * 1080 * 60 * 0.02 = 2.48 Mbps

        let width = display_info.desktop_coordinates.width() as u64;
        let height = display_info.desktop_coordinates.height() as u64;
        let fps = if self.video_fps == 0 {
            60
        } else {
            self.video_fps
        } as u64;
        let pixels_per_second = width * height * fps;

        // Linear interpolation for BPP
        // bpp = max_bpp - (video_quality / 63.0) * (max_bpp - min_bpp)
        let max_bpp = 0.20f64;
        let min_bpp = 0.02f64;
        let quality_ratio = (self.video_quality as f64 / 63.0).clamp(0.0, 1.0);
        let bpp = max_bpp - quality_ratio * (max_bpp - min_bpp);

        let mut bps = (pixels_per_second as f64 * bpp) as u32;

        // Cap bps at 100 Mbps to prevent OpenH264 error and excessive bandwidth usage
        let max_bps = 100_000_000;
        if bps > max_bps {
            bps = max_bps;
        }

        encoder_settings.bps = bps;
        // Default GOP to 120 frames. WebRTC recovers from loss via PLI/FIR,
        // so we don't need a tight periodic IDR; a wider GOP spreads the
        // ~25 KB IDR cost over more frames and reduces bandwidth spikes.
        // gop=0 would mean only the first frame is IDR, causing
        // seek/recovery failures on packet loss.
        encoder_settings.gop = 120;
        encoder_settings
    }

    pub fn get_vp8_encoder_settings(&self) -> VpxEncoderSettings {
        if let Some(ref vp8_encoder) = self.vp8_encoder {
            return vp8_encoder.clone();
        }
        // use video_quality to create a default vp8 encoder settings
        VpxEncoderSettings {
            quality: self.video_quality,
            ..Default::default()
        }
    }

    pub fn get_vp9_encoder_settings(&self) -> VpxEncoderSettings {
        if let Some(ref vp9_encoder) = self.vp9_encoder {
            return vp9_encoder.clone();
        }
        // use video_quality to create a default vp9 encoder settings
        VpxEncoderSettings {
            quality: self.video_quality,
            ..Default::default()
        }
    }

    pub fn get_av1_encoder_settings(&self) -> Av1EncoderSettings {
        if let Some(ref av1_encoder) = self.av1_encoder {
            return av1_encoder.clone();
        }
        // use video_quality to create a default av1 encoder settings
        // video_quality: 0-63 (lower is better) -> rav1e quantizer: 0-255 (lower is better)
        Av1EncoderSettings {
            quality: (self.video_quality as f64 / 63.0 * 255.0).clamp(0.0, 255.0) as u32,
            ..Default::default()
        }
    }
}

impl Default for DeskSettings {
    fn default() -> Self {
        Self {
            enable_d3d_debug: false,
            video_device_name: String::new(),
            video_quality: 22,
            adaptive_web_page_resolution: false,
            video_zoom_ratio: 100,
            video_fps: 60,
            show_mouse: true,
            image_capture: None,
            audio_capture: None,
            video_encoder: None,
            audio_device: None,
            audio_encoder: None,
            x264_encoder: None,
            h264_encoder: None,
            vp8_encoder: None,
            vp9_encoder: None,
            opus_encoder: None,
            av1_encoder: None,
            private_screen: PrivateScreenSettings::default(),
            display_name: None,
            wayland_control_mode: None,
            enable_dirty_rect: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::model::image_capture::DisplayRect;

    use super::*;

    #[test]
    fn test_get_h264_encoder_settings() {
        let mut settings = DeskSettings::default();
        let mut display_info = DisplayInfo {
            desktop_coordinates: DisplayRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            ..Default::default()
        };
        settings.video_fps = 60;

        // Test quality 0 (highest)
        settings.video_quality = 0;
        let h264_0 = settings.get_h264_encoder_settings(&display_info);
        // 1920 * 1080 * 60 * 0.20 = 24,883,200
        assert!(h264_0.bps > 24_000_000 && h264_0.bps < 25_000_000);

        // Test quality 63 (lowest)
        settings.video_quality = 63;
        let h264_63 = settings.get_h264_encoder_settings(&display_info);
        // 1920 * 1080 * 60 * 0.02 = 2,488,320
        assert!(h264_63.bps > 2_400_000 && h264_63.bps < 2_500_000);

        // Test with 4K resolution
        display_info.desktop_coordinates = DisplayRect {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 2160,
        };
        settings.video_quality = 0;
        let h264_4k = settings.get_h264_encoder_settings(&display_info);
        // 3840 * 2160 * 60 * 0.20 = 99,532,800
        assert_eq!(h264_4k.bps, 99_532_800);

        // Test with custom high resolution (should hit cap)
        display_info.desktop_coordinates.right = 4000;
        let h264_hit_cap = settings.get_h264_encoder_settings(&display_info);
        assert_eq!(h264_hit_cap.bps, 100_000_000);
    }
}

#[cfg(test)]
mod wincode_tests {
    use super::*;
    use crate::model::audio_capture::{AudioDataFlow, SelectedAudioDevice};
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    /// `DeskSettings` reaches the deepest type graph in the facade —
    /// `Option<SelectedAudioDevice>` plus five `Option<*EncoderSettings>`
    /// plus nested `PrivateScreenSettings` plus nested
    /// `Option<HotkeySettings>`. Build an instance that *populates*
    /// every nested Option so any missed-derive surfaces here as a
    /// compile or round-trip failure rather than as a silent
    /// encode-empty further downstream.
    #[test]
    fn desk_settings_round_trips_wincode_with_nested_options_populated() {
        let original = DeskSettings {
            enable_d3d_debug: true,
            video_device_name: r"\\.\DISPLAY2".to_string(),
            video_quality: 33,
            adaptive_web_page_resolution: true,
            video_zoom_ratio: 75,
            video_fps: 45,
            show_mouse: false,
            image_capture: Some("dxgi".to_string()),
            audio_capture: Some("wasapi".to_string()),
            video_encoder: Some("X264".to_string()),
            audio_device: Some(SelectedAudioDevice {
                audio_data_flow: AudioDataFlow::Render,
                audio_device_id: Some("device-1".to_string()),
            }),
            audio_encoder: Some("OPUS".to_string()),
            x264_encoder: Some(X264EncoderSettings {
                quality: 18,
                gop: 240,
            }),
            h264_encoder: Some(H264EncoderSettings {
                bps: 8_000_000,
                gop: 60,
            }),
            vp8_encoder: Some(VpxEncoderSettings {
                bps: 5_000_000,
                quality: 30,
            }),
            vp9_encoder: Some(VpxEncoderSettings {
                bps: 6_000_000,
                quality: 28,
            }),
            opus_encoder: Some(OpusEncoderSettings {
                sample_rate: 48_000,
                channels: 2,
                application: "Voip".to_string(),
            }),
            av1_encoder: Some(Av1EncoderSettings {
                quality: 100,
                speed: 8,
            }),
            private_screen: PrivateScreenSettings {
                image_path: Some(r"C:\private.png".to_string()),
                window_style: Some(0x12345678),
                window_ex_style: None,
                hotkey: Some(HotkeySettings {
                    fsmodifiers: 5,
                    vk: 'P' as u32,
                }),
                enabled: true,
            },
            display_name: Some("\\\\.\\DISPLAY2".to_string()),
            wayland_control_mode: Some("portal".to_string()),
            enable_dirty_rect: false,
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: DeskSettings = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back, original);
    }

    /// The all-`None` extreme — every Option field is `None`, every
    /// nested Option-inside-PrivateScreenSettings is `None` — must
    /// also round-trip. This is the case JSON deserialisation hits
    /// via `#[serde(default)]`; wincode's positional encoding makes
    /// it a separate concern (each Option gets its tag byte on the
    /// wire regardless of the default).
    #[test]
    fn desk_settings_default_round_trips_wincode() {
        let original = DeskSettings::default();
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: DeskSettings = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back, original);
    }

    /// `video_device_name = ""` means "no display selected yet" and is
    /// the legal default for a fresh install. The browser must surface
    /// a chooser before media starts; the daemon, when relaying
    /// `StartMediaPayload`, must map this empty string to `None` so
    /// the worker treats it as "use whatever the OS hands me" rather
    /// than as a literal display name.
    #[test]
    fn desk_settings_video_device_name_default_is_empty_string() {
        let s = DeskSettings::default();
        assert_eq!(s.video_device_name, "");
    }

    /// Old TOML / JSON written before the v4 rename carries a numeric
    /// `video_device_index` field. Struct-level `#[serde(default)]` lets
    /// serde drop the unknown field silently and populate the renamed
    /// `video_device_name` with its empty-string default. This keeps
    /// users on existing on-disk config from hard-failing the daemon
    /// boot — they will be prompted to pick a display in the browser
    /// on their next session.
    #[test]
    fn desk_settings_serde_ignores_unknown_video_device_index() {
        let raw = r#"{"video_device_index": 7}"#;
        let s: DeskSettings = serde_json::from_str(raw).expect("decode");
        assert_eq!(s.video_device_name, "");
    }
}
