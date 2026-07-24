use std::{backtrace::Backtrace, sync::Arc};

use desk_signal_facade::model::{
    desk_settings::DeskSettings,
    image_capture::{DisplayInfo, DisplayRect},
};
use desk_utils::error::DeskErrorCode;
use windows::Win32::{
    Foundation::{GENERIC_ALL, HMODULE, LUID, RECT},
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_REFERENCE,
            D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_9_1, D3D_FEATURE_LEVEL_10_0,
            D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            D3D11_SRV_DIMENSION_TEXTURE2D, Fxc::D3DCompile,
        },
        Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VERTEX_BUFFER,
            D3D11_BLEND_DESC, D3D11_BLEND_INV_DEST_ALPHA, D3D11_BLEND_INV_SRC_ALPHA,
            D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA, D3D11_BOX,
            D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_NEVER,
            D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG,
            D3D11_CREATE_DEVICE_FLAG, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FLOAT32_MAX,
            D3D11_INPUT_ELEMENT_DESC, D3D11_INPUT_PER_VERTEX_DATA, D3D11_SAMPLER_DESC,
            D3D11_SDK_VERSION, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SUBRESOURCE_DATA,
            D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            D3D11_USAGE_STAGING, D3D11_VIEWPORT, D3D11CreateDevice, ID3D11BlendState, ID3D11Device,
            ID3D11DeviceContext, ID3D11InputLayout, ID3D11PixelShader, ID3D11RenderTargetView,
            ID3D11SamplerState, ID3D11Texture2D, ID3D11VertexShader,
        },
        Dxgi::{
            Common::{
                DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_R32G32B32_FLOAT,
            },
            CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ERROR_ACCESS_LOST,
            DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_INVALID_CALL, DXGI_ERROR_NOT_FOUND,
            DXGI_ERROR_WAIT_TIMEOUT, DXGI_MAP_READ, DXGI_MAPPED_RECT, DXGI_OUTDUPL_DESC,
            DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, DXGI_OUTPUT_DESC,
            DXGI_RESOURCE_PRIORITY_MAXIMUM, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice,
            IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, IDXGISurface,
        },
        Gdi::{DISPLAY_DEVICE_STATE_FLAGS, DISPLAY_DEVICEW, EnumDisplayDevicesW},
    },
    Media::MediaFoundation::{MF_FLOAT2, MF_FLOAT3},
    System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetProcessWindowStation,
        OpenInputDesktop, SetThreadDesktop,
    },
};
use windows_core::{Interface, PCWSTR, s};

use crate::{
    error::CaptureError,
    image_capture::dxgi_compose,
    image_capture::windows::enum_display_resolutions,
    model::image_capture::CursorSyncData,
    model::image_capture::{
        CaptureRequest, CaptureResult, CursorCaptureMode, DirtyRect, ImageCapture,
        ImageCaptureType, ImageInfo, ImageOutputEnumerator, ImageType,
    },
};

/// Placeholder image returned when content_changed == false (Map was not called).
struct EmptyImageInfo;

impl ImageInfo for EmptyImageInfo {
    fn get_type(&self) -> ImageType {
        ImageType::BGRA
    }
    fn get_data(&self) -> &[u8] {
        &[]
    }
    fn get_width(&self) -> u32 {
        0
    }
    fn get_height(&self) -> u32 {
        0
    }
}

pub(crate) enum FrameAcquisitionResult<'a> {
    ContentFrame(SceenFrame<'a>),
    NoContentChange,
    /// The acquired desktop texture's size diverges from
    /// `dup_output_desc.ModeDesc`. Callers (`DxgiImageCapture::capture`)
    /// drop the `ScreenOutput` so the next tick rebuilds it against
    /// the new resolution. This covers the path where a mid-session
    /// resolution change does *not* surface as
    /// `DXGI_ERROR_ACCESS_LOST` (some drivers keep the duplication
    /// alive but report a smaller / larger texture for the new mode).
    Rebuild,
}

/// Identity key used to deduplicate `CursorSyncData` emissions.
/// Includes `screen_width` / `screen_height` so that a resolution
/// change forces a fresh emission even when the cursor shape is
/// unchanged — otherwise the front-end's stale `screen_width` makes
/// the cursor sprite scale incorrectly after a mid-session resize.
/// The `Embedded` variant marks frames where the OS has composited
/// the cursor pixel into the captured desktop image (DXGI
/// software-cursor mode); the front-end then hides its own CSS
/// cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DxgiCursorFingerprint {
    Hidden,
    Embedded,
    Shape {
        id: u64,
        screen_width: u32,
        screen_height: u32,
    },
}

/// Detects the DXGI software-cursor path: the OS has composited the
/// cursor pixel into the desktop image returned by
/// `AcquireNextFrame`. Mirrors the heuristic used by WebRTC's
/// `dxgi_output_duplicator.cc`:
///
/// * `LastMouseUpdateTime != 0` — the duplication API actually has
///   a pointer-position update to report for the current frame.
///   When this is zero the pointer info is stale / absent and we
///   cannot make any claim.
/// * `!PointerPosition.Visible` — when visible, the pointer is
///   delivered as a separate hardware/overlay plane and the
///   acquired desktop image contains no cursor pixels. When the
///   pointer-position update says "not visible" *despite* an
///   update being reported, the OS has switched to software cursor
///   mode and the cursor is now part of the desktop image.
///
/// The two predicates together rule out the "no pointer info this
/// frame" case (where `Visible` defaults to false simply because
/// nothing changed). Pulled out as a pure function so the state
/// machine in `get_frame` is unit-testable without a live DXGI
/// pipeline.
fn frame_contains_embedded_cursor(frame_info: &DXGI_OUTDUPL_FRAME_INFO) -> bool {
    frame_info.LastMouseUpdateTime != 0 && !frame_info.PointerPosition.Visible.as_bool()
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VERTEX {
    pub pos: MF_FLOAT3,
    pub tex_coord: MF_FLOAT2,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

pub const POINTER_SHAPE_TYPE_MONOCHROME: u32 = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
pub const POINTER_SHAPE_TYPE_COLOR: u32 = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32;
pub const POINTER_SHAPE_TYPE_MASKED_COLOR: u32 =
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;

/// FIXME: can not find the type of XMFLOAT2 and XMFLOAT3 in windows-rs, use MF_FLOAT3 and MF_FLOAT2 instead
pub const VERTICES: [VERTEX; 6] = [
    VERTEX {
        pos: MF_FLOAT3 {
            x: -1.0,
            y: -1.0,
            z: 0.0,
        },
        tex_coord: MF_FLOAT2 { x: 0.0, y: 1.0 },
    },
    VERTEX {
        pos: MF_FLOAT3 {
            x: -1.0,
            y: 1.0,
            z: 0.0,
        },
        tex_coord: MF_FLOAT2 { x: 0.0, y: 0.0 },
    },
    VERTEX {
        pos: MF_FLOAT3 {
            x: 1.0,
            y: -1.0,
            z: 0.0,
        },
        tex_coord: MF_FLOAT2 { x: 1.0, y: 1.0 },
    },
    VERTEX {
        pos: MF_FLOAT3 {
            x: 1.0,
            y: -1.0,
            z: 0.0,
        },
        tex_coord: MF_FLOAT2 { x: 1.0, y: 1.0 },
    },
    VERTEX {
        pos: MF_FLOAT3 {
            x: -1.0,
            y: 1.0,
            z: 0.0,
        },
        tex_coord: MF_FLOAT2 { x: 0.0, y: 0.0 },
    },
    VERTEX {
        pos: MF_FLOAT3 {
            x: 1.0,
            y: 1.0,
            z: 0.0,
        },
        tex_coord: MF_FLOAT2 { x: 1.0, y: 0.0 },
    },
];

pub const NUMVERTICES: u32 = VERTICES.len() as u32;
pub const BPP: i32 = 4;

mod capture;
mod enumeration;
mod manager;
mod output;

pub use capture::{DxgiImageCapture, DxgiImageOutputEnumerator};
pub use enumeration::{from_dxgi_output_desc, from_rect};
pub use manager::{ScreenRecordManager, ScreenRecordManagerArc};
pub use output::{SceenFrame, ScreenOutput};

pub(crate) use enumeration::{
    adapter_name_from_desc, enumerate_all_outputs, output_device_name, select_output_by_name,
};

#[cfg(test)]
pub(crate) use enumeration::{find_device_name_index, order_adapters_by_default_luid};

#[cfg(test)]
mod tests;
