use serde::{Deserialize, Serialize};
use windows::Win32::{
    Foundation::RECT,
    Graphics::{
        Dxgi::{
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, DXGI_OUTPUT_DESC,
        },
        Gdi::{DISPLAY_DEVICE_STATE_FLAGS, DISPLAY_DEVICEW, EnumDisplayDevicesW},
    },
    Media::MediaFoundation::{MF_FLOAT2, MF_FLOAT3},
};
use windows_core::PCWSTR;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DisplayRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl From<RECT> for DisplayRect {
    fn from(value: RECT) -> Self {
        DisplayRect {
            left: value.left,
            top: value.top,
            right: value.right,
            bottom: value.bottom,
        }
    }
}
/// Display Info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub device_name: String,
    pub display_device_name: Option<String>,
    pub desktop_coordinates: DisplayRect,
    pub attached_to_desktop: bool,
    pub rotation: i32,
}

impl DisplayInfo {
    pub fn from_digx_output_desc(output_desc: &DXGI_OUTPUT_DESC) -> Self {
        log::debug!(
            "Converting DXGI_OUTPUT_DESC to DisplayInfo, output_desc: {:?}",
            output_desc
        );

        let null_char_index = output_desc
            .DeviceName
            .iter()
            .position(|&item| item == 0u16)
            .unwrap_or(output_desc.DeviceName.len());
        let device_name: String =
            String::from_utf16_lossy(&output_desc.DeviceName[..null_char_index]);

        let mut display_device = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            DeviceName: [0u16; 32],
            DeviceString: [0u16; 128],
            StateFlags: DISPLAY_DEVICE_STATE_FLAGS(0),
            DeviceID: [0u16; 128],
            DeviceKey: [0u16; 128],
        };
        let succeed = unsafe {
            EnumDisplayDevicesW(
                PCWSTR::from_raw(output_desc.DeviceName.as_ptr()),
                0,
                &mut display_device,
                0,
            )
        };
        let display_device_name = if succeed.as_bool() {
            log::info!(
                "Successfully enumerated display device: {:?}",
                display_device
            );
            let null_char_index = display_device
                .DeviceString
                .iter()
                .position(|&item| item == 0u16)
                .unwrap_or(output_desc.DeviceName.len());
            let name: String =
                String::from_utf16_lossy(&display_device.DeviceString[..null_char_index]);

            log::debug!("Display device name: {}", name);
            Some(name)
        } else {
            None
        };
        let desktop_coordinates = output_desc.DesktopCoordinates.into();
        let attached_to_desktop = output_desc.AttachedToDesktop.as_bool();
        let rotation = output_desc.Rotation.0;

        log::info!(
            "Found output, name={}, display_device_name={:?}, desktop_coordinates={:?}, attached_to_desktop={}, rotation={}",
            device_name,
            display_device_name,
            desktop_coordinates,
            attached_to_desktop,
            rotation
        );
        DisplayInfo {
            device_name,
            display_device_name,
            desktop_coordinates,
            attached_to_desktop,
            rotation,
        }
    }
}

impl From<DXGI_OUTPUT_DESC> for DisplayInfo {
    fn from(output_desc: DXGI_OUTPUT_DESC) -> Self {
        DisplayInfo::from_digx_output_desc(&output_desc)
    }
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
