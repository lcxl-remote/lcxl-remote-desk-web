use std::time::Duration;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::model::{audio_capture::SelectedAudioDevice, image_capture::DisplayInfo};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Deserialize,
    Serialize,
    ToSchema,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
#[serde(rename_all = "snake_case")]
pub enum LinuxInputControlMode {
    Auto,
    None,
    Uinput,
    Portal,
}

impl LinuxInputControlMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "none" => Some(Self::None),
            "uinput" => Some(Self::Uinput),
            "portal" => Some(Self::Portal),
            _ => None,
        }
    }

    pub const fn resolve(self, wayland: bool) -> Self {
        match (self, wayland) {
            (Self::Auto, true) => Self::Portal,
            (Self::Auto, false) => Self::Uinput,
            (mode, _) => mode,
        }
    }

    pub const fn needs_portal_input(self) -> bool {
        matches!(self, Self::Portal)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Uinput => "uinput",
            Self::Portal => "portal",
        }
    }
}

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

/// AV1 encoder settings (SVT-AV1)
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
    /// CRF (constant rate factor), 0-63, default is 35. Lower is better quality.
    pub crf: u32,
    /// SVT-AV1 preset, 0-13, default is 12. Higher is faster (lower quality).
    pub preset: u32,
    /// Real-time low-delay (RTC) mode. Default true for remote desktop so the
    /// encoder emits a packet per input frame instead of buffering a deep
    /// look-ahead window.
    pub rtc: bool,
    /// CBR target bitrate (bps) used in RTC/low-delay mode. `0` means
    /// "auto-derive" from resolution / fps / `crf` via `default_video_bps`.
    /// In RTC mode SVT-AV1 only supports the low-delay prediction structure
    /// under CBR rate control (`crf`/CQP forces a random-access structure
    /// that is incompatible with the RTC flag and aborts the encoder), so a
    /// positive target bitrate is required there. Unused when `rtc == false`
    /// (the CRF / random-access path uses `crf` instead).
    pub target_bps: u32,
}

impl Default for Av1EncoderSettings {
    fn default() -> Self {
        Self {
            crf: 35,
            preset: 12, // remote desktop needs the fast end of the preset range
            rtc: true,
            target_bps: 0, // 0 = auto-derive from resolution / fps / crf
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

/// Derives a default video bitrate (bps) from resolution, frame rate
/// and the 0-63 `video_quality` knob via bits-per-pixel interpolation.
///
/// - quality 0 (highest) → 0.20 bpp; quality 63 (lowest) → 0.02 bpp.
/// - For 1080p @ 60 fps that spans 24.8 Mbps (quality 0) down to
///   2.48 Mbps (quality 63).
/// - Capped at 100 Mbps to prevent OpenH264 errors and excessive
///   bandwidth usage.
///
/// Used both for the OpenH264 default target bitrate and as the
/// initial (loosest) VBV ceiling of the x264 constrained-quality
/// encoder.
pub fn default_video_bps(width: u64, height: u64, fps: u64, video_quality: u32) -> u32 {
    let pixels_per_second = width * height * fps;

    // Linear interpolation for BPP
    // bpp = max_bpp - (video_quality / 63.0) * (max_bpp - min_bpp)
    let max_bpp = 0.20f64;
    let min_bpp = 0.02f64;
    let quality_ratio = (video_quality as f64 / 63.0).clamp(0.0, 1.0);
    let bpp = max_bpp - quality_ratio * (max_bpp - min_bpp);

    let bps = (pixels_per_second as f64 * bpp) as u32;

    // Cap bps at 100 Mbps to prevent OpenH264 error and excessive bandwidth usage
    bps.min(100_000_000)
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
    /// Whether the daemon's REMB-driven adaptive bitrate cap is active
    /// for this connection. Defaults to `true`. Session-scoped state:
    /// it takes effect per connection (initialised from the offer's
    /// desk settings, live-toggled via `UpdateDeskSettings` for the
    /// originating connection only) and is not persisted server-side —
    /// the browser keeps the user's preference. Distinct from the
    /// browser-side adaptive *quality* loop: the cap only trims
    /// bitrate spikes, quality stays untouched.
    pub adaptive_bitrate: bool,
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

        let width = display_info.desktop_coordinates.width() as u64;
        let height = display_info.desktop_coordinates.height() as u64;
        let fps = if self.video_fps == 0 {
            60
        } else {
            self.video_fps
        } as u64;

        encoder_settings.bps = default_video_bps(width, height, fps, self.video_quality);
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

    pub fn get_av1_encoder_settings(&self, display_info: &DisplayInfo) -> Av1EncoderSettings {
        let width = display_info.desktop_coordinates.width() as u64;
        let height = display_info.desktop_coordinates.height() as u64;
        // Mirror `get_h264_encoder_settings`: a 0 fps would derive a 0 bitrate
        // and CBR rate control needs a positive target, so fall back to 60.
        let fps = if self.video_fps == 0 {
            60
        } else {
            self.video_fps
        } as u64;

        if let Some(ref av1_encoder) = self.av1_encoder {
            let mut settings = av1_encoder.clone();
            // RTC mode is CBR; a 0 target means "auto-derive". `crf` drives the
            // bits-per-pixel point exactly like `video_quality` (both 0-63).
            if settings.target_bps == 0 {
                settings.target_bps = default_video_bps(width, height, fps, settings.crf);
            }
            return settings;
        }
        // Use video_quality to create default av1 encoder settings. Both
        // video_quality and SVT-AV1 CRF are 0-63 (lower is better), so the knob
        // maps onto `crf` directly; the same knob also seeds the CBR target.
        Av1EncoderSettings {
            crf: self.video_quality.min(63),
            target_bps: default_video_bps(width, height, fps, self.video_quality),
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
            adaptive_bitrate: true,
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

    #[test]
    fn av1_settings_defaults_are_real_time_oriented() {
        let s = Av1EncoderSettings::default();
        assert_eq!(s.crf, 35);
        assert_eq!(s.preset, 12);
        assert!(
            s.rtc,
            "remote desktop AV1 must default to RTC low-delay mode"
        );
    }

    fn av1_display_info(width: i32, height: i32) -> DisplayInfo {
        DisplayInfo {
            desktop_coordinates: DisplayRect {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            ..Default::default()
        }
    }

    #[test]
    fn get_av1_encoder_settings_maps_video_quality_to_crf_directly() {
        let with_quality = |video_quality: u32| DeskSettings {
            video_quality,
            ..Default::default()
        };
        let di = av1_display_info(1920, 1080);

        // video_quality and CRF share the 0-63 range, so the mapping is identity.
        let lo = with_quality(0).get_av1_encoder_settings(&di);
        assert_eq!(lo.crf, 0);
        assert_eq!(lo.preset, Av1EncoderSettings::default().preset);
        assert!(lo.rtc);

        assert_eq!(with_quality(63).get_av1_encoder_settings(&di).crf, 63);

        // Out-of-range video_quality is clamped to the CRF ceiling.
        assert_eq!(with_quality(1000).get_av1_encoder_settings(&di).crf, 63);
    }

    #[test]
    fn get_av1_encoder_settings_prefers_explicit_settings() {
        let settings = DeskSettings {
            av1_encoder: Some(Av1EncoderSettings {
                crf: 20,
                preset: 6,
                rtc: false,
                target_bps: 7_000_000,
            }),
            ..Default::default()
        };
        let got = settings.get_av1_encoder_settings(&av1_display_info(1920, 1080));
        assert_eq!(got.crf, 20);
        assert_eq!(got.preset, 6);
        assert!(!got.rtc);
        // An explicit non-zero target is passed through untouched.
        assert_eq!(got.target_bps, 7_000_000);
    }

    /// RTC mode is CBR and needs a positive target bitrate. A `target_bps` of
    /// 0 (the default "auto" sentinel) must be derived from resolution / fps /
    /// crf rather than left at 0 — a 0 target would abort the CBR encoder.
    #[test]
    fn get_av1_encoder_settings_auto_derives_target_bps() {
        // Default path (no explicit av1_encoder): quality knob seeds the target.
        let defaults = DeskSettings {
            video_quality: 22,
            video_fps: 60,
            ..Default::default()
        };
        let got = defaults.get_av1_encoder_settings(&av1_display_info(1920, 1080));
        assert!(
            got.target_bps > 0,
            "auto target must be derived from resolution/fps/crf, got 0"
        );
        assert_eq!(
            got.target_bps,
            default_video_bps(1920, 1080, 60, 22),
            "default path target must match the shared bitrate formula"
        );

        // Explicit settings with target_bps == 0 must also be auto-derived.
        let explicit = DeskSettings {
            video_fps: 30,
            av1_encoder: Some(Av1EncoderSettings {
                crf: 35,
                preset: 12,
                rtc: true,
                target_bps: 0,
            }),
            ..Default::default()
        };
        let got = explicit.get_av1_encoder_settings(&av1_display_info(1280, 720));
        assert_eq!(got.target_bps, default_video_bps(1280, 720, 30, 35));
    }

    /// A `video_fps` of 0 would derive a 0 bitrate; like the h264 path it must
    /// fall back to 60 so CBR rate control still gets a positive target.
    #[test]
    fn get_av1_encoder_settings_fps_zero_falls_back() {
        let settings = DeskSettings {
            video_quality: 22,
            video_fps: 0,
            ..Default::default()
        };
        let got = settings.get_av1_encoder_settings(&av1_display_info(1920, 1080));
        assert!(
            got.target_bps > 0,
            "fps=0 must fall back to 60 and derive a positive target, got 0"
        );
        assert_eq!(got.target_bps, default_video_bps(1920, 1080, 60, 22));
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
                crf: 40,
                preset: 8,
                rtc: false,
                target_bps: 6_000_000,
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
            adaptive_bitrate: false,
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
