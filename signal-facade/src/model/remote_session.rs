use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use wincode::{SchemaRead, SchemaWrite};

use desk_utils::error::DeskErrorCode;

use crate::model::{
    audio_capture::SelectedAudioDevice, desk_settings::DeskSettings,
    media_capability::VideoEncoderId,
};

/// Deserialize an explicitly present nullable field. Do not add
/// `#[serde(default)]` at call sites: a missing key must remain a protocol
/// error, while JSON `null` maps to `None`.
pub fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, SchemaWrite, SchemaRead,
)]
pub enum AudioEncoderId {
    #[serde(rename = "Opus")]
    Opus,
}

impl AudioEncoderId {
    pub const fn setting_name(self) -> &'static str {
        match self {
            Self::Opus => "Opus",
        }
    }

    pub fn from_setting_name(value: &str) -> Option<Self> {
        value.eq_ignore_ascii_case("opus").then_some(Self::Opus)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema, SchemaWrite, SchemaRead)]
pub struct AudioPipelineSettings {
    pub audio_capture: String,
    pub audio_device: SelectedAudioDevice,
    pub audio_encoder: AudioEncoderId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema, SchemaWrite, SchemaRead)]
pub struct RemoteSessionSettings {
    pub image_capture: String,
    pub video_device_name: String,
    pub show_mouse: bool,
    pub video_encoder: VideoEncoderId,
    pub video_quality: u32,
    pub video_fps: u32,
    pub enable_dirty_rect: bool,
    pub adaptive_bitrate: bool,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    #[schema(required = true)]
    pub audio: Option<AudioPipelineSettings>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SuggestedSessionSettings {
    pub capture_audio: bool,
    pub image_capture: Option<String>,
    pub video_device_name: Option<String>,
    pub show_mouse: bool,
    pub video_encoder: Option<VideoEncoderId>,
    pub video_quality: u32,
    pub video_fps: u32,
    pub enable_dirty_rect: bool,
    pub adaptive_bitrate: bool,
    pub audio_capture: Option<String>,
    pub audio_device: Option<SelectedAudioDevice>,
    pub audio_encoder: Option<AudioEncoderId>,
}

impl SuggestedSessionSettings {
    pub fn from_host_settings(settings: &DeskSettings, audio_capable: bool) -> Self {
        Self {
            capture_audio: audio_capable,
            image_capture: settings
                .image_capture
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            video_device_name: (!settings.video_device_name.is_empty())
                .then(|| settings.video_device_name.clone()),
            show_mouse: settings.show_mouse,
            video_encoder: settings
                .video_encoder
                .as_deref()
                .and_then(VideoEncoderId::from_setting_name),
            video_quality: settings.video_quality,
            video_fps: settings.video_fps,
            enable_dirty_rect: settings.enable_dirty_rect,
            adaptive_bitrate: settings.adaptive_bitrate,
            audio_capture: settings.audio_capture.clone(),
            audio_device: settings.audio_device.clone(),
            audio_encoder: settings
                .audio_encoder
                .as_deref()
                .and_then(AudioEncoderId::from_setting_name),
        }
    }
}

impl RemoteSessionSettings {
    /// Merge controller-owned session knobs onto the host-only base settings.
    /// This is the only bridge into the existing worker configuration while the
    /// host-only encoder/private-screen subsettings remain local.
    pub fn merge_into_host_settings(&self, host: &DeskSettings) -> DeskSettings {
        let mut merged = host.clone();
        merged.image_capture = Some(self.image_capture.clone());
        merged.video_device_name = self.video_device_name.clone();
        merged.show_mouse = self.show_mouse;
        merged.video_encoder = Some(self.video_encoder.setting_name().to_string());
        merged.video_quality = self.video_quality;
        merged.video_fps = self.video_fps;
        merged.enable_dirty_rect = self.enable_dirty_rect;
        merged.adaptive_bitrate = self.adaptive_bitrate;
        match &self.audio {
            Some(audio) => {
                merged.audio_capture = Some(audio.audio_capture.clone());
                merged.audio_device = Some(audio.audio_device.clone());
                merged.audio_encoder = Some(audio.audio_encoder.setting_name().to_string());
            }
            None => {
                merged.audio_capture = None;
                merged.audio_device = None;
                merged.audio_encoder = None;
            }
        }
        merged
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionSettingApplyMode {
    Unsupported,
    OfferOnly,
    Apply,
    Reconnect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SessionSettingsCapabilities {
    pub capture_audio: SessionSettingApplyMode,
    pub image_capture: SessionSettingApplyMode,
    pub video_device_name: SessionSettingApplyMode,
    pub show_mouse: SessionSettingApplyMode,
    pub video_encoder: SessionSettingApplyMode,
    pub video_quality: SessionSettingApplyMode,
    pub video_fps: SessionSettingApplyMode,
    pub enable_dirty_rect: SessionSettingApplyMode,
    pub adaptive_bitrate: SessionSettingApplyMode,
    pub audio_capture: SessionSettingApplyMode,
    pub audio_device: SessionSettingApplyMode,
    pub audio_encoder: SessionSettingApplyMode,
}

impl SessionSettingsCapabilities {
    pub fn desktop(audio_capable: bool) -> Self {
        let audio = if audio_capable {
            SessionSettingApplyMode::Apply
        } else {
            SessionSettingApplyMode::Unsupported
        };
        Self {
            capture_audio: audio,
            image_capture: SessionSettingApplyMode::Apply,
            video_device_name: SessionSettingApplyMode::Apply,
            show_mouse: SessionSettingApplyMode::Apply,
            video_encoder: SessionSettingApplyMode::Apply,
            video_quality: SessionSettingApplyMode::Apply,
            video_fps: SessionSettingApplyMode::Apply,
            enable_dirty_rect: SessionSettingApplyMode::Apply,
            adaptive_bitrate: SessionSettingApplyMode::Apply,
            audio_capture: audio,
            audio_device: audio,
            audio_encoder: audio,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IceCandidatePayload {
    pub connection_epoch: String,
    pub candidate: RTCIceCandidateInit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ConnectionEpochPayload {
    pub connection_epoch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CloseRemoteSessionPayload {
    pub connection_epoch: String,
    /// `false` tears down only the current PeerConnection so the same
    /// signaling connection can immediately admit a replacement epoch.
    /// `true` also releases logical-session resources such as privacy screen,
    /// host activity and admission state.
    pub finalize_logical_connection: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ApplyRemoteSessionSettings {
    pub connection_epoch: String,
    pub settings: RemoteSessionSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum VideoSettingsEffect {
    Unchanged,
    AppliedLive,
    Restarted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AudioSettingsEffect {
    Unchanged,
    Started,
    Stopped,
    Restarted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionSettingsEffect {
    Unchanged,
    NeedsReconnect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RemoteSessionSettingsEffects {
    pub video: VideoSettingsEffect,
    pub audio: AudioSettingsEffect,
    pub connection: ConnectionSettingsEffect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RemoteSessionSettingsRuntimeOverrides {
    pub adaptive_video_quality: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RemoteSessionSettingsFieldError {
    pub field: String,
    pub code: DeskErrorCode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RemoteSessionSettingsApplied {
    pub connection_epoch: String,
    pub effects: RemoteSessionSettingsEffects,
    pub baseline_settings: RemoteSessionSettings,
    pub runtime_overrides: RemoteSessionSettingsRuntimeOverrides,
    pub errors: Vec<RemoteSessionSettingsFieldError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UpdateAdaptiveVideoQuality {
    pub connection_epoch: String,
    pub video_quality: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SystemAudioCaptureState {
    Off,
    Starting,
    Active,
    Restarting,
    Denied,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SystemAudioCaptureStateData {
    pub connection_epoch: String,
    pub state: SystemAudioCaptureState,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    #[schema(required = true)]
    pub accepted_audio: Option<AudioPipelineSettings>,
    pub resolved_audio_device_id: Option<String>,
    pub error_code: Option<DeskErrorCode>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::audio_capture::AudioDataFlow;

    fn remote_settings(audio: Option<AudioPipelineSettings>) -> RemoteSessionSettings {
        RemoteSessionSettings {
            image_capture: "WGC".into(),
            video_device_name: r"\\.\DISPLAY1".into(),
            show_mouse: true,
            video_encoder: VideoEncoderId::X264,
            video_quality: 22,
            video_fps: 30,
            enable_dirty_rect: true,
            adaptive_bitrate: true,
            audio,
        }
    }

    #[test]
    fn remote_settings_requires_audio_key_but_accepts_explicit_null() {
        let missing = serde_json::json!({
            "image_capture": "WGC",
            "video_device_name": r"\\.\DISPLAY1",
            "show_mouse": true,
            "video_encoder": "X264",
            "video_quality": 22,
            "video_fps": 30,
            "enable_dirty_rect": true,
            "adaptive_bitrate": true
        });
        assert!(serde_json::from_value::<RemoteSessionSettings>(missing).is_err());

        let encoded = serde_json::to_value(remote_settings(None)).unwrap();
        assert!(encoded.get("audio").is_some());
        assert!(encoded["audio"].is_null());
        assert!(serde_json::from_value::<RemoteSessionSettings>(encoded).is_ok());
    }

    #[test]
    fn audio_pipeline_round_trips_as_complete_object() {
        let audio = AudioPipelineSettings {
            audio_capture: "WASAPI".into(),
            audio_device: SelectedAudioDevice {
                audio_data_flow: AudioDataFlow::Render,
                audio_device_id: None,
            },
            audio_encoder: AudioEncoderId::Opus,
        };
        let encoded = serde_json::to_value(remote_settings(Some(audio.clone()))).unwrap();
        let decoded: RemoteSessionSettings = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.audio, Some(audio));
    }

    #[test]
    fn audio_state_requires_accepted_audio_key() {
        let missing = serde_json::json!({
            "connection_epoch": "epoch",
            "state": "off",
            "resolved_audio_device_id": null,
            "error_code": null
        });
        assert!(serde_json::from_value::<SystemAudioCaptureStateData>(missing).is_err());

        let value = SystemAudioCaptureStateData {
            connection_epoch: "epoch".into(),
            state: SystemAudioCaptureState::Off,
            accepted_audio: None,
            resolved_audio_device_id: None,
            error_code: None,
        };
        assert!(serde_json::to_value(value).unwrap()["accepted_audio"].is_null());
    }
}
