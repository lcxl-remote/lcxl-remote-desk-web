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

pub fn from_rect(rect: &RECT) -> DisplayRect {
    DisplayRect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

pub fn from_dxgi_output_desc(output_desc: &DXGI_OUTPUT_DESC) -> DisplayInfo {
    log::debug!(
        "Converting DXGI_OUTPUT_DESC to DisplayInfo, output_desc: {:?}",
        output_desc
    );

    let null_char_index = output_desc
        .DeviceName
        .iter()
        .position(|&item| item == 0u16)
        .unwrap_or(output_desc.DeviceName.len());
    let device_name: String = String::from_utf16_lossy(&output_desc.DeviceName[..null_char_index]);

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
        // DEBUG, not INFO: this is on the per-frame `get_current_output`
        // path for any caller that re-queries display info each tick;
        // emitting at INFO floods the log. Static metadata on the
        // adapter is logged once at capture construction in
        // `ScreenRecordManager`, which is the appropriate place for
        // operator-visible enumeration output.
        log::debug!(
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
    let desktop_coordinates = from_rect(&output_desc.DesktopCoordinates);
    let attached_to_desktop = output_desc.AttachedToDesktop.as_bool();
    let rotation = output_desc.Rotation.0;

    // DEBUG, not INFO: see rationale on the `Successfully enumerated
    // display device` log just above. Per-frame callers would otherwise
    // emit this at the OS refresh rate.
    log::debug!(
        "Found output, name={}, display_device_name={:?}, desktop_coordinates={:?}, attached_to_desktop={}, rotation={}",
        device_name,
        display_device_name,
        desktop_coordinates,
        attached_to_desktop,
        rotation
    );

    let resolutions = enum_display_resolutions(&device_name).unwrap_or_default();

    DisplayInfo {
        device_name,
        display_device_name,
        desktop_coordinates,
        resolutions,
        attached_to_desktop,
        rotation,
    }
}

// ============================================================================
// Cross-adapter output enumeration (phase 1)
// ============================================================================

/// One DXGI output joined with the adapter that owns it. The flat
/// ordering of a `Vec<EnumeratedOutput>` is the order presented to the
/// frontend dropdown: the default hardware adapter is placed first
/// (so existing single-GPU users see no behavior change), then the
/// remaining adapters follow in `IDXGIFactory1::EnumAdapters1` order.
/// Selection is by `DeskSettings::video_device_name` against the GDI
/// device name embedded in each entry's `DXGI_OUTPUT_DESC.DeviceName`.
#[derive(Clone)]
pub(crate) struct EnumeratedOutput {
    pub adapter_index: u32,
    pub local_output_index: u32,
    pub adapter: IDXGIAdapter1,
    pub desc: DXGI_OUTPUT_DESC,
    pub adapter_desc: DXGI_ADAPTER_DESC1,
}

/// Extract the GDI device name (`\\.\DISPLAYn`) from a DXGI output
/// descriptor. The 32-wchar field is null-terminated.
pub(crate) fn output_device_name(desc: &DXGI_OUTPUT_DESC) -> String {
    let nul = desc
        .DeviceName
        .iter()
        .position(|&c| c == 0u16)
        .unwrap_or(desc.DeviceName.len());
    String::from_utf16_lossy(&desc.DeviceName[..nul])
}

/// Pure device-name selection: given a flat slice of GDI device names
/// in flat-enumeration order, return the position whose entry exactly
/// matches `requested`. Pure (no DXGI fixtures) so the
/// not-found / empty-string / IDD-hint branches are exhaustively
/// testable. The IDD hint is generated on every not-found error because
/// missing IDD enumeration is the most common reason a user-selected
/// display fails to resolve through DXGI; we cannot prove the missing
/// entry is in fact an IDD, so the message is worded as "is the
/// likely cause if you selected a virtual display."
pub(crate) fn find_device_name_index(
    names: &[String],
    requested: &str,
) -> Result<usize, CaptureError> {
    if requested.is_empty() {
        return CaptureError::custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "video_device_name is empty: no display has been selected. \
             Open the desktop dialog in the browser and pick a display \
             before starting media.",
        );
    }
    if let Some(idx) = names.iter().position(|n| n == requested) {
        return Ok(idx);
    }
    let summary = if names.is_empty() {
        "(none)".to_string()
    } else {
        names
            .iter()
            .map(|n| format!("{:?}", n))
            .collect::<Vec<_>>()
            .join(", ")
    };
    CaptureError::custom_error(
        DeskErrorCode::INVALID_PARAMS,
        &format!(
            "device_name {:?} not enumerated by DXGI; enumerated: [{}]. \
             IDD virtual displays are not exposed through DXGI \
             (EnumAdapters does not allocate a dedicated IDXGIAdapter \
             for IDD devices); switch the capture backend to WGC if \
             this is a virtual display.",
            requested, summary
        ),
    )
}

/// Pure ordering: returns the permutation (indices into `adapter_luids`)
/// that places `default_luid` first when present, and keeps the rest in
/// their original factory order. If `default_luid` is `None` or its LUID
/// is not found in `adapter_luids`, the identity permutation is
/// returned. Extracted so it is unit-testable without DXGI fixtures.
fn order_adapters_by_default_luid(
    default_luid: Option<LUID>,
    adapter_luids: &[LUID],
) -> Vec<usize> {
    let identity: Vec<usize> = (0..adapter_luids.len()).collect();
    let Some(target) = default_luid else {
        return identity;
    };
    let Some(promoted) = adapter_luids
        .iter()
        .position(|l| l.LowPart == target.LowPart && l.HighPart == target.HighPart)
    else {
        return identity;
    };
    let mut order = Vec::with_capacity(adapter_luids.len());
    order.push(promoted);
    for (i, _) in adapter_luids.iter().enumerate() {
        if i != promoted {
            order.push(i);
        }
    }
    order
}

/// Capture the LUID of the adapter that `D3D11CreateDevice(None, HARDWARE)`
/// would pick, so cross-adapter enumeration can promote it to flat
/// position 0 and keep single-default-adapter users seeing the same
/// dropdown order. Returns `None` (without erroring) if the probe
/// fails — callers fall back to factory order.
fn probe_default_adapter_luid() -> Option<LUID> {
    let driver_types: [D3D_DRIVER_TYPE; 3] = [
        D3D_DRIVER_TYPE_HARDWARE,
        D3D_DRIVER_TYPE_WARP,
        D3D_DRIVER_TYPE_REFERENCE,
    ];
    let feature_levels: [D3D_FEATURE_LEVEL; 4] = [
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
        D3D_FEATURE_LEVEL_9_1,
    ];
    let mut device: Option<ID3D11Device> = None;
    for driver_type in driver_types {
        let r = unsafe {
            D3D11CreateDevice(
                None,
                driver_type,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        };
        if r.is_ok() {
            break;
        }
    }
    let device = device?;
    let dxgi_device = device.cast::<IDXGIDevice>().ok()?;
    let adapter = unsafe { dxgi_device.GetParent::<IDXGIAdapter>().ok()? };
    let adapter1 = adapter.cast::<IDXGIAdapter1>().ok()?;
    let desc = unsafe { adapter1.GetDesc1().ok()? };
    Some(desc.AdapterLuid)
}

/// Enumerate every output across every DXGI adapter. See
/// [`EnumeratedOutput`] for the flat ordering contract.
pub(crate) fn enumerate_all_outputs() -> Result<Vec<EnumeratedOutput>, CaptureError> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;

    // Phase 1: collect every adapter (preserve factory order).
    struct AdapterEntry {
        adapter: IDXGIAdapter1,
        desc: DXGI_ADAPTER_DESC1,
        luid: LUID,
        output_descs: Vec<DXGI_OUTPUT_DESC>,
    }
    let mut adapters: Vec<AdapterEntry> = Vec::new();
    let mut adapter_idx: u32 = 0;
    loop {
        let r = unsafe { factory.EnumAdapters1(adapter_idx) };
        let adapter = match r {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(CaptureError::from(e)),
        };
        let desc = unsafe { adapter.GetDesc1() }?;
        let luid = desc.AdapterLuid;
        let mut output_descs: Vec<DXGI_OUTPUT_DESC> = Vec::new();
        let mut out_idx: u32 = 0;
        loop {
            let r = unsafe { adapter.EnumOutputs(out_idx) };
            let output = match r {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(CaptureError::from(e)),
            };
            let output_desc = unsafe { output.GetDesc() }?;
            output_descs.push(output_desc);
            out_idx += 1;
        }
        adapters.push(AdapterEntry {
            adapter,
            desc,
            luid,
            output_descs,
        });
        adapter_idx += 1;
    }

    // Phase 2: reorder adapters so the default hardware adapter is
    // first. Preserves dropdown ordering for users who only ever saw
    // default-adapter outputs.
    let default_luid = probe_default_adapter_luid();
    let luids: Vec<LUID> = adapters.iter().map(|e| e.luid).collect();
    let order = order_adapters_by_default_luid(default_luid, &luids);

    // Phase 3: flatten according to the new order.
    let mut flat: Vec<EnumeratedOutput> = Vec::new();
    for (new_adapter_idx, &orig_idx) in order.iter().enumerate() {
        let entry = &adapters[orig_idx];
        for (local_idx, output_desc) in entry.output_descs.iter().enumerate() {
            flat.push(EnumeratedOutput {
                adapter_index: new_adapter_idx as u32,
                local_output_index: local_idx as u32,
                adapter: entry.adapter.clone(),
                desc: *output_desc,
                adapter_desc: entry.desc,
            });
        }
    }
    log::info!(
        "enumerate_all_outputs: {} adapter(s), {} output(s) total, default_luid={}",
        adapters.len(),
        flat.len(),
        match default_luid {
            Some(l) => format!("Some({:?})", (l.LowPart, l.HighPart)),
            None => "None".to_string(),
        }
    );
    Ok(flat)
}

/// Resolve a GDI device name against the flat enumeration. On
/// failure, the error message includes the requested name, every
/// enumerated device_name, a per-adapter summary (for multi-GPU
/// triage), and the IDD-not-on-DXGI hint generated by
/// [`find_device_name_index`].
pub(crate) fn select_output_by_name<'a>(
    entries: &'a [EnumeratedOutput],
    device_name: &str,
) -> Result<&'a EnumeratedOutput, CaptureError> {
    let names: Vec<String> = entries
        .iter()
        .map(|e| output_device_name(&e.desc))
        .collect();
    match find_device_name_index(&names, device_name) {
        Ok(idx) => Ok(&entries[idx]),
        Err(e) => {
            // Re-wrap to append the per-adapter summary, which the pure
            // helper cannot compute (it does not see EnumeratedOutput).
            CaptureError::custom_error(
                DeskErrorCode::INVALID_PARAMS,
                &format!(
                    "{} adapter_summary: ({})",
                    e,
                    build_adapter_summary(entries)
                ),
            )
        }
    }
}

/// Render a `"adapter[i]='Name' K output(s); ..."` string describing
/// the flat enumeration. Helper for `select_output` error messages.
fn build_adapter_summary(entries: &[EnumeratedOutput]) -> String {
    let mut summary = String::new();
    let mut current_adapter: i64 = -1;
    let mut current_count: u32 = 0;
    let mut current_name = String::new();
    for e in entries {
        if e.adapter_index as i64 != current_adapter {
            if current_adapter >= 0 {
                summary.push_str(&format!(
                    "adapter[{}]='{}' {} output(s); ",
                    current_adapter, current_name, current_count
                ));
            }
            current_adapter = e.adapter_index as i64;
            current_count = 0;
            current_name = adapter_name_from_desc(&e.adapter_desc);
        }
        current_count += 1;
    }
    if current_adapter >= 0 {
        summary.push_str(&format!(
            "adapter[{}]='{}' {} output(s)",
            current_adapter, current_name, current_count
        ));
    }
    summary
}

/// Convert `DXGI_ADAPTER_DESC1::Description` ([u16; 128]) to a String,
/// stopping at the first NUL.
pub(crate) fn adapter_name_from_desc(desc: &DXGI_ADAPTER_DESC1) -> String {
    let null = desc
        .Description
        .iter()
        .position(|&c| c == 0u16)
        .unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..null])
}

pub struct ScreenRecordManager {
    pub device: ID3D11Device,
    pub device_context: ID3D11DeviceContext,
    pub dxgi_adapter: IDXGIAdapter,
    pub blend_state: ID3D11BlendState,

    pub vertex_shader: ID3D11VertexShader,
    pub input_layout: ID3D11InputLayout,
    pub pixel_shader: ID3D11PixelShader,
    pub sampler_linear: [Option<ID3D11SamplerState>; 1],
}

impl ScreenRecordManager {
    pub fn set_thread_input_desktop() -> Result<(), CaptureError> {
        unsafe {
            let result = GetProcessWindowStation();
            if let Err(err) = result {
                log::error!("GetProcessWindowStation failed, error: {}", err);
            } else if let Ok(station) = result {
                log::info!("GetProcessWindowStation success, handle: {:?}", station);
            }

            let current_deskop = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
            )?;
            log::info!("OpenInputDesktop success, handle: {:?}", current_deskop);
            SetThreadDesktop(current_deskop)?;
            let result = CloseDesktop(current_deskop);
            if let Err(err) = result {
                log::warn!("Failed to close desktop, ignore, error: {}", err);
            }
        };
        Ok(())
    }

    /// make_rtv creates a render target view for the given back buffer texture.
    pub fn make_rtv(
        &self,
        back_buffer: &ID3D11Texture2D,
    ) -> Result<[Option<ID3D11RenderTargetView>; 1], CaptureError> {
        // Create a render target view
        let rtv = unsafe {
            let mut rtv = None;
            self.device
                .CreateRenderTargetView(back_buffer, None, Some(&mut rtv))?;
            let rtv = [rtv];
            // Set new render target
            self.device_context.OMSetRenderTargets(Some(&rtv), None);
            rtv
        };
        Ok(rtv)
    }

    /// Set new viewport
    pub fn set_view_port(&self, width: u32, height: u32) {
        let mut viewport = D3D11_VIEWPORT::default();
        viewport.Width = width as f32;
        viewport.Height = height as f32;
        viewport.MinDepth = 0.0;
        viewport.MaxDepth = 1.0;
        viewport.TopLeftX = 0.0;
        viewport.TopLeftY = 0.0;
        unsafe { self.device_context.RSSetViewports(Some(&[viewport])) };
    }

    /// Initialize shaders and input layout
    pub fn init_shaders(
        device: &ID3D11Device,
        device_context: &ID3D11DeviceContext,
    ) -> Result<(ID3D11VertexShader, ID3D11InputLayout, ID3D11PixelShader), CaptureError> {
        //https://learn.microsoft.com/zh-cn/windows/win32/api/d3dcompiler/nf-d3dcompiler-d3dcompile
        let vertex_shader_code = include_str!("shaders/VertexShader.hlsl");
        let pixel_shader_code = include_str!("shaders/PixelShader.hlsl");

        let mut vertex_shader = None;
        let mut error_msg = None;
        let compile_result = unsafe {
            D3DCompile(
                vertex_shader_code.as_ptr() as *const _,
                vertex_shader_code.len(),
                s!("VertexShader.hlsl"),
                None,
                None,
                s!("VS"),
                s!("vs_4_0_level_9_1"),
                0,
                0,
                &mut vertex_shader,
                Some(&mut error_msg),
            )
        };
        if let Err(complie_error) = compile_result
            && let Some(blob) = error_msg
        {
            // ansi format string?
            let blob_array = unsafe {
                core::slice::from_raw_parts(
                    blob.GetBufferPointer() as *const u8,
                    blob.GetBufferSize(),
                )
            };
            let error_message = String::from_utf8_lossy(blob_array);
            log::error!("Vertex Shader Compile Error: {}", error_message);
            return Err(CaptureError::from(complie_error));
        }

        let mut pixel_shader = None;
        let mut error_msg = None;
        let compile_result = unsafe {
            D3DCompile(
                pixel_shader_code.as_ptr() as *const _,
                pixel_shader_code.len(),
                s!("PixelShader.hlsl"),
                None,
                None,
                s!("PS"),
                s!("ps_4_0_level_9_1"),
                0,
                0,
                &mut pixel_shader,
                Some(&mut error_msg),
            )
        };
        if let Err(complie_error) = compile_result
            && let Some(blob) = error_msg
        {
            // ansi format string?
            let blob_array = unsafe {
                core::slice::from_raw_parts(
                    blob.GetBufferPointer() as *const u8,
                    blob.GetBufferSize(),
                )
            };
            let error_message = String::from_utf8_lossy(blob_array);
            log::error!("Pixel Shader Compile Error: {}", error_message);
            return Err(CaptureError::from(complie_error));
        }
        let vertex_shader = vertex_shader.unwrap();
        let vertex_shader_blob = unsafe {
            core::slice::from_raw_parts(
                vertex_shader.GetBufferPointer() as *const u8,
                vertex_shader.GetBufferSize(),
            )
        };
        let mut vertex_shader = None;
        unsafe { device.CreateVertexShader(vertex_shader_blob, None, Some(&mut vertex_shader)) }?;
        let vertex_shader = vertex_shader.unwrap();

        let layout = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("POSITION"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32B32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("TEXCOORD"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 12,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
        ];
        let mut input_layout = None;
        unsafe { device.CreateInputLayout(&layout, vertex_shader_blob, Some(&mut input_layout)) }?;
        let input_layout = input_layout.unwrap();
        unsafe { device_context.IASetInputLayout(&input_layout) };

        let pixel_shader = pixel_shader.unwrap();
        let pixel_shader_blob = unsafe {
            core::slice::from_raw_parts(
                pixel_shader.GetBufferPointer() as *const u8,
                pixel_shader.GetBufferSize(),
            )
        };

        let mut pixel_shader = None;
        unsafe { device.CreatePixelShader(pixel_shader_blob, None, Some(&mut pixel_shader)) }?;

        let pixel_shader = pixel_shader.unwrap();
        Ok((vertex_shader, input_layout, pixel_shader))
    }

    pub fn new(settings: &DeskSettings) -> Result<Arc<Self>, CaptureError> {
        Self::set_thread_input_desktop()?;
        let flags = Self::device_flags(settings);

        let driver_types: [D3D_DRIVER_TYPE; 3] = [
            D3D_DRIVER_TYPE_HARDWARE,
            D3D_DRIVER_TYPE_WARP,
            D3D_DRIVER_TYPE_REFERENCE,
        ];
        let feature_levels: [D3D_FEATURE_LEVEL; 4] = [
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
            D3D_FEATURE_LEVEL_9_1,
        ];

        let mut device: Option<ID3D11Device> = None;
        let mut device_context: Option<ID3D11DeviceContext> = None;
        let mut result = Ok(());

        for driver_type in driver_types {
            result = unsafe {
                D3D11CreateDevice(
                    None,
                    driver_type,
                    HMODULE::default(),
                    flags,
                    Some(&feature_levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut device_context),
                )
            };
            if let Err(error) = &result {
                log::warn!(
                    "Failed to create device with driver type {:?}, err: {}",
                    driver_type,
                    error
                );
            } else {
                break;
            }
        }
        result?;

        let device = device.unwrap();
        let device_context = device_context.unwrap();

        let dxgi_device = device.cast::<IDXGIDevice>()?;
        let dxgi_adapter = unsafe { dxgi_device.GetParent::<IDXGIAdapter>() }?;

        Self::init_d3d_pipeline(device, device_context, dxgi_adapter)
    }

    /// Build a `ScreenRecordManager` whose D3D11 device is created on a
    /// specific adapter — required for `IDXGIOutputDuplication`, which
    /// demands the device and output share an adapter. Used by the
    /// cross-adapter path in `DxgiImageCapture::new` (see
    /// [`enumerate_all_outputs`]).
    pub fn new_with_adapter(
        settings: &DeskSettings,
        adapter: &IDXGIAdapter1,
    ) -> Result<Arc<Self>, CaptureError> {
        Self::set_thread_input_desktop()?;
        let flags = Self::device_flags(settings);

        // Cast IDXGIAdapter1 → IDXGIAdapter explicitly so we never rely
        // on windows-rs Param trait inference at the call site.
        let adapter_base: IDXGIAdapter = adapter.cast::<IDXGIAdapter>()?;

        let feature_levels: [D3D_FEATURE_LEVEL; 4] = [
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
            D3D_FEATURE_LEVEL_9_1,
        ];

        let mut device: Option<ID3D11Device> = None;
        let mut device_context: Option<ID3D11DeviceContext> = None;
        // MSDN: when pAdapter is non-NULL, DriverType MUST be
        // D3D_DRIVER_TYPE_UNKNOWN.
        let create_result = unsafe {
            D3D11CreateDevice(
                Some(&adapter_base),
                windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                flags,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut device_context),
            )
        };
        if let Err(err) = create_result {
            let adapter_desc = unsafe { adapter.GetDesc1() }.ok();
            let adapter_name = adapter_desc
                .as_ref()
                .map(adapter_name_from_desc)
                .unwrap_or_else(|| "<GetDesc1 failed>".to_string());
            let (lo, hi) = adapter_desc
                .as_ref()
                .map(|d| (d.AdapterLuid.LowPart, d.AdapterLuid.HighPart))
                .unwrap_or((0, 0));
            return CaptureError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "D3D11CreateDevice with explicit adapter='{}' (LUID={:#x}:{:#x}) failed: {} ({:?})",
                    adapter_name,
                    hi,
                    lo,
                    err.message(),
                    err.code()
                ),
            );
        }

        let device = device.unwrap();
        let device_context = device_context.unwrap();
        Self::init_d3d_pipeline(device, device_context, adapter_base)
    }

    fn device_flags(settings: &DeskSettings) -> D3D11_CREATE_DEVICE_FLAG {
        let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        if settings.enable_d3d_debug {
            log::info!("Enable d3d debug flag");
            flags |= D3D11_CREATE_DEVICE_DEBUG;
        }
        flags
    }

    fn init_d3d_pipeline(
        device: ID3D11Device,
        device_context: ID3D11DeviceContext,
        dxgi_adapter: IDXGIAdapter,
    ) -> Result<Arc<Self>, CaptureError> {
        let mut samp_desc = D3D11_SAMPLER_DESC::default();
        samp_desc.Filter = D3D11_FILTER_MIN_MAG_MIP_LINEAR;
        samp_desc.AddressU = D3D11_TEXTURE_ADDRESS_CLAMP;
        samp_desc.AddressV = D3D11_TEXTURE_ADDRESS_CLAMP;
        samp_desc.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
        samp_desc.ComparisonFunc = D3D11_COMPARISON_NEVER;
        samp_desc.MinLOD = 0.0;
        samp_desc.MaxLOD = D3D11_FLOAT32_MAX;
        let mut sampler_linear = None;
        unsafe { device.CreateSamplerState(&samp_desc, Some(&mut sampler_linear)) }?;
        let sampler_linear = [sampler_linear];

        let mut blend_state_desc = D3D11_BLEND_DESC::default();
        blend_state_desc.AlphaToCoverageEnable = false.into();
        blend_state_desc.IndependentBlendEnable = false.into();
        blend_state_desc.RenderTarget[0].BlendEnable = true.into();
        blend_state_desc.RenderTarget[0].SrcBlend = D3D11_BLEND_SRC_ALPHA;
        blend_state_desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
        blend_state_desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
        // big thanks to https://github.com/MirrorX-Desktop/MirrorX/blob/master/mirrorx_core/src/component/desktop/windows/duplicator.rs#L1013C51-L1013C80
        blend_state_desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_INV_DEST_ALPHA;
        blend_state_desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_ONE;
        blend_state_desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
        blend_state_desc.RenderTarget[0].RenderTargetWriteMask =
            D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;

        let mut blend_state = None;
        unsafe { device.CreateBlendState(&blend_state_desc, Some(&mut blend_state)) }?;
        let blend_state = blend_state.unwrap();

        let (vertex_shader, input_layout, pixel_shader) =
            ScreenRecordManager::init_shaders(&device, &device_context)?;
        log::info!("ScreenRecordManager initialized successfully");
        Ok(Arc::new(ScreenRecordManager {
            device,
            device_context,
            dxgi_adapter,
            blend_state,
            vertex_shader,
            input_layout,
            pixel_shader,
            sampler_linear,
        }))
    }
}

pub trait ScreenRecordManagerArc {
    fn get_screen_output(&self, output_index: u32) -> Result<ScreenOutput, CaptureError>;
}

impl ScreenRecordManagerArc for Arc<ScreenRecordManager> {
    fn get_screen_output(&self, output_index: u32) -> Result<ScreenOutput, CaptureError> {
        ScreenOutput::new(self.clone(), output_index)
    }
}

pub struct ScreenOutput {
    pub manager: Arc<ScreenRecordManager>,
    pub output_index: u32,
    pub dup_output: IDXGIOutputDuplication,
    pub dup_output_desc: DXGI_OUTDUPL_DESC,
    pub copy_buffer_texture_2d: ID3D11Texture2D,
    pub copy_buffer_surface: IDXGISurface,
    pub pointer_shape_buffer: Vec<u8>,
    pub last_mouse_update_time: i64,
    pub pointer_position: Point,
    pub pointer_visible: bool,
    pub pointer_shape_info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    /// Persistent composition surface — kept across frames so move +
    /// dirty regions can be merged onto the previous-frame state per
    /// the MSDN sample. Cleared to opaque black at construction.
    pub render_target_texture_2d: ID3D11Texture2D,
    pub rtv: [Option<ID3D11RenderTargetView>; 1],
    /// Scratch surface used when relocating a move rect within the
    /// persistent RT. D3D11 forbids `CopySubresourceRegion` with src
    /// and dst on the same subresource, so MSDN's sample routes the
    /// move via an intermediate texture; lazy-allocated on first
    /// non-zero move-count frame.
    pub move_surf: Option<ID3D11Texture2D>,
    /// Intermediate surface holding "RT + cursor". Cursor is drawn
    /// here instead of on the persistent RT so the cursor does not
    /// pollute the next frame's non-dirty regions (cursor moves do
    /// not generate dirty rects, so RT-resident cursors would leave
    /// shadow trails).
    pub cursor_overlay_texture: ID3D11Texture2D,
    pub cursor_overlay_rtv: [Option<ID3D11RenderTargetView>; 1],
    /// CPU-side vertex scratch (grows monotonically) — six vertices
    /// per dirty rect.
    pub dirty_vertex_scratch: Vec<VERTEX>,
    /// GPU-side vertex buffer for `compose_dirty`. Grown on demand
    /// (rounded up to a power of two) and rewritten with
    /// `UpdateSubresource` each frame.
    pub dirty_vertex_buffer: Option<windows::Win32::Graphics::Direct3D11::ID3D11Buffer>,
    pub dirty_vertex_buffer_capacity_verts: u32,
    /// The render-target rect the cursor was last drawn into (after
    /// the `draw_mouse_into` call completes). Compared against the
    /// next frame's would-be cursor rect to drive `build_dirty_hint`'s
    /// cursor-delta state machine.
    pub last_cursor_rect: Option<DirtyRect>,
    pub metadata_buffer: Vec<u8>,
    /// When `true`, `get_frame` skips the MSDN dirty/move composition
    /// path and instead `CopyResource`s the entire acquired desktop
    /// texture into the persistent RT each frame. Toggled by the
    /// `LCXL_DXGI_FULL_BLIT` environment variable at `ScreenOutput`
    /// construction time — diagnostic A/B switch only, not exposed
    /// to the UI.
    pub full_frame_blit: bool,
    /// `true` when the most recent `AcquireNextFrame` reported a
    /// cursor that the OS has already composited into the desktop
    /// image (DXGI software-cursor mode). Computed via
    /// `frame_contains_embedded_cursor` and used to (a) force
    /// `content_changed` on cursor-only events so the video stream
    /// follows the embedded cursor, (b) tell the front-end to hide
    /// its CSS cursor, and (c) force the YUV dirty hint to `None` so
    /// the cursor's old position is repainted under full-frame
    /// conversion.
    pub last_frame_embedded: bool,
}

impl ScreenOutput {
    pub fn new(
        screen_record_manager: Arc<ScreenRecordManager>,
        output_index: u32,
    ) -> Result<Self, CaptureError> {
        let output = unsafe { screen_record_manager.dxgi_adapter.EnumOutputs(output_index) }?;

        let dxgi_output_desc = unsafe { output.GetDesc() }?;
        let output1 = output.cast::<IDXGIOutput1>()?;
        // get the device from the manager and pass it to DuplicateOutput
        let pdevice = &screen_record_manager.device;

        let dup_output = unsafe { output1.DuplicateOutput(pdevice) }?;
        let dup_output_desc = unsafe { dup_output.GetDesc() };
        log::info!(
            "output_index {}, dxgi_output_desc {:?}, dup_output_desc {:?}",
            output_index,
            dxgi_output_desc,
            dup_output_desc
        );

        // Staging buffer/texture
        let mut copy_buffer_desc: D3D11_TEXTURE2D_DESC = unsafe { std::mem::zeroed() };

        copy_buffer_desc.Width = dup_output_desc.ModeDesc.Width;
        copy_buffer_desc.Height = dup_output_desc.ModeDesc.Height;
        copy_buffer_desc.MipLevels = 1;
        copy_buffer_desc.ArraySize = 1;
        //The format must be DXGI_FORMAT_B8G8R8A8_UNORM, see https://learn.microsoft.com/zh-cn/windows/win32/direct3ddxgi/desktop-dup-api#updating-the-desktop-image-data
        copy_buffer_desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        copy_buffer_desc.SampleDesc.Count = 1;
        copy_buffer_desc.SampleDesc.Quality = 0;
        copy_buffer_desc.Usage = D3D11_USAGE_STAGING;
        copy_buffer_desc.BindFlags = 0;
        copy_buffer_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        copy_buffer_desc.MiscFlags = 0;

        // create a texture to hold the screen capture
        let mut copy_buffer_texture_2d = None;
        unsafe {
            screen_record_manager.device.CreateTexture2D(
                &copy_buffer_desc,
                None,
                Some(&mut copy_buffer_texture_2d),
            )
        }?;
        let copy_buffer_texture_2d = copy_buffer_texture_2d.unwrap();
        unsafe { copy_buffer_texture_2d.SetEvictionPriority(DXGI_RESOURCE_PRIORITY_MAXIMUM.0) };
        let copy_buffer_surface = copy_buffer_texture_2d.cast::<IDXGISurface>()?;

        // Create persistent composition render target.
        let render_target_texture_2d = ScreenOutput::create_render_target_texture(
            &screen_record_manager.device,
            dup_output_desc.ModeDesc.Width,
            dup_output_desc.ModeDesc.Height,
        )?;
        let rtv = screen_record_manager.make_rtv(&render_target_texture_2d)?;

        // Cursor overlay surface — same dimensions as the RT.
        let cursor_overlay_texture = ScreenOutput::create_render_target_texture(
            &screen_record_manager.device,
            dup_output_desc.ModeDesc.Width,
            dup_output_desc.ModeDesc.Height,
        )?;
        let cursor_overlay_rtv = screen_record_manager.make_rtv(&cursor_overlay_texture)?;

        // Newly created D3D11 textures have undefined content. Clear
        // both composition surfaces to opaque black so the first few
        // frames — before driver-reported dirty + move regions cover
        // the screen — present deterministic darkness rather than
        // garbage pixels (see plan §1 trade-off note).
        let clear_black = [0.0_f32, 0.0_f32, 0.0_f32, 1.0_f32];
        unsafe {
            if let Some(rtv0) = rtv[0].as_ref() {
                screen_record_manager
                    .device_context
                    .ClearRenderTargetView(rtv0, &clear_black);
            }
            if let Some(cursor_rtv0) = cursor_overlay_rtv[0].as_ref() {
                screen_record_manager
                    .device_context
                    .ClearRenderTargetView(cursor_rtv0, &clear_black);
            }
        }

        screen_record_manager.set_view_port(
            dup_output_desc.ModeDesc.Width,
            dup_output_desc.ModeDesc.Height,
        );
        Ok(ScreenOutput {
            manager: screen_record_manager,
            output_index,
            dup_output,
            dup_output_desc,
            copy_buffer_texture_2d,
            copy_buffer_surface,
            pointer_shape_buffer: vec![],
            last_mouse_update_time: 0,
            pointer_position: Point::default(),
            pointer_visible: false,
            pointer_shape_info: DXGI_OUTDUPL_POINTER_SHAPE_INFO::default(),
            render_target_texture_2d,
            rtv,
            move_surf: None,
            cursor_overlay_texture,
            cursor_overlay_rtv,
            dirty_vertex_scratch: Vec::new(),
            dirty_vertex_buffer: None,
            dirty_vertex_buffer_capacity_verts: 0,
            last_cursor_rect: None,
            metadata_buffer: vec![],
            // full_frame_blit is the default since the
            // 2026-05-21 capture-resolution + cursor-residue fix —
            // the legacy per-rect compose path is reachable only via
            // the inverse opt-out env var `LCXL_DXGI_DIRTY_COMPOSE`
            // (for A/B diagnostics). Per-rect compose leaves cursor
            // ghosts on software-cursor frames because SyncNative
            // mode never populates `cursor_after.rect`, so
            // `build_dirty_hint` cannot include cursor move deltas.
            full_frame_blit: {
                let env_val = std::env::var("LCXL_DXGI_DIRTY_COMPOSE").ok();
                let force_dirty = dxgi_compose::parse_env_flag(env_val.as_deref());
                if force_dirty {
                    log::warn!(
                        "[DXGI] LCXL_DXGI_DIRTY_COMPOSE enabled (raw={:?}) — \
                         output_index={} will use legacy per-rect compose; may \
                         exhibit cursor / resolution-change ghosting.",
                        env_val,
                        output_index
                    );
                }
                !force_dirty
            },
            last_frame_embedded: false,
        })
    }

    /// Reads the move-rect and dirty-rect metadata reported by
    /// `IDXGIOutputDuplication` for the current frame. Returns a pair
    /// of independently-owned byte buffers so the caller can decode
    /// them at leisure without worrying about the underlying scratch
    /// buffer being overwritten on the second Get* call.
    ///
    /// The DXGI API reuses a single caller-supplied buffer for both
    /// queries (move first, dirty second); we copy the move bytes out
    /// before the dirty query runs.
    fn read_frame_metadata(
        &mut self,
        total_bytes: u32,
    ) -> Result<(Vec<u8>, Vec<u8>), CaptureError> {
        self.metadata_buffer.resize(total_bytes as usize, 0);

        let mut move_bytes_used: u32 = 0;
        let mut move_raw: Vec<u8> = Vec::new();
        let move_query = unsafe {
            self.dup_output.GetFrameMoveRects(
                total_bytes,
                self.metadata_buffer.as_mut_ptr() as *mut DXGI_OUTDUPL_MOVE_RECT,
                &mut move_bytes_used,
            )
        };
        if move_query.is_ok() {
            move_raw = self.metadata_buffer[..move_bytes_used as usize].to_vec();
        } else {
            log::trace!(
                "GetFrameMoveRects returned non-success; treating move list as empty: {:?}",
                move_query
            );
        }

        let mut dirty_bytes_used: u32 = 0;
        let mut dirty_raw: Vec<u8> = Vec::new();
        let dirty_query = unsafe {
            self.dup_output.GetFrameDirtyRects(
                total_bytes,
                self.metadata_buffer.as_mut_ptr() as *mut RECT,
                &mut dirty_bytes_used,
            )
        };
        if dirty_query.is_ok() {
            dirty_raw = self.metadata_buffer[..dirty_bytes_used as usize].to_vec();
        } else {
            log::trace!(
                "GetFrameDirtyRects returned non-success; treating dirty list as empty: {:?}",
                dirty_query
            );
        }

        Ok((move_raw, dirty_raw))
    }

    /// Lazily creates the scratch surface used to route move rects.
    /// D3D11 forbids using one subresource as both source and
    /// destination of `CopySubresourceRegion`, so MSDN's sample
    /// shuttles each move via an intermediate texture.
    fn ensure_move_surf(&mut self) -> Result<(), CaptureError> {
        if self.move_surf.is_some() {
            return Ok(());
        }
        let tex = ScreenOutput::create_render_target_texture(
            &self.manager.device,
            self.dup_output_desc.ModeDesc.Width,
            self.dup_output_desc.ModeDesc.Height,
        )?;
        self.move_surf = Some(tex);
        Ok(())
    }

    /// Applies every move rect to the persistent RT in place: copy
    /// source region into `move_surf`, then copy `move_surf` back to
    /// the destination region. Identity rotation only (see
    /// `dxgi_compose::set_move_rect`).
    fn copy_move_rects(&mut self, moves: &[DXGI_OUTDUPL_MOVE_RECT]) -> Result<(), CaptureError> {
        if moves.is_empty() {
            return Ok(());
        }
        self.ensure_move_surf()?;
        let move_surf = self
            .move_surf
            .as_ref()
            .expect("ensure_move_surf must have populated move_surf");
        for mv in moves {
            let (src, dst) = dxgi_compose::set_move_rect(mv);
            let mut src_box = D3D11_BOX::default();
            src_box.left = src.left.max(0) as u32;
            src_box.top = src.top.max(0) as u32;
            src_box.right = src.right.max(0) as u32;
            src_box.bottom = src.bottom.max(0) as u32;
            src_box.front = 0;
            src_box.back = 1;
            unsafe {
                // RT[src] → move_surf[src]
                self.manager.device_context.CopySubresourceRegion(
                    move_surf,
                    0,
                    src_box.left,
                    src_box.top,
                    0,
                    &self.render_target_texture_2d,
                    0,
                    Some(&src_box),
                );
                // move_surf[src] → RT[dst]
                self.manager.device_context.CopySubresourceRegion(
                    &self.render_target_texture_2d,
                    0,
                    dst.left.max(0) as u32,
                    dst.top.max(0) as u32,
                    0,
                    move_surf,
                    0,
                    Some(&src_box),
                );
            }
        }
        Ok(())
    }

    /// Ensures the dirty-rect vertex buffer can hold at least
    /// `verts_needed` vertices, growing in powers of two and starting
    /// at `NUMVERTICES * 16` to amortise reallocation.
    fn ensure_dirty_vertex_buffer(&mut self, verts_needed: u32) -> Result<(), CaptureError> {
        if verts_needed <= self.dirty_vertex_buffer_capacity_verts
            && self.dirty_vertex_buffer.is_some()
        {
            return Ok(());
        }
        let mut cap = (NUMVERTICES * 16).max(1);
        while cap < verts_needed {
            cap = cap.saturating_mul(2);
        }
        let mut desc = D3D11_BUFFER_DESC::default();
        desc.Usage = D3D11_USAGE_DEFAULT;
        desc.ByteWidth = (size_of::<VERTEX>() as u32) * cap;
        desc.BindFlags = D3D11_BIND_VERTEX_BUFFER.0 as u32;
        desc.CPUAccessFlags = 0;
        let mut buf = None;
        unsafe {
            self.manager
                .device
                .CreateBuffer(&desc, None, Some(&mut buf))
        }?;
        self.dirty_vertex_buffer = buf;
        self.dirty_vertex_buffer_capacity_verts = cap;
        Ok(())
    }

    /// Composes the dirty regions into the persistent RT by drawing
    /// six vertices per rect with the acquired desktop image bound
    /// as the source texture. Replaces the old full-quad blit
    /// (`draw_desktop`) so non-dirty pixels keep their previous
    /// content as MSDN requires.
    fn compose_dirty_rects(
        &mut self,
        dirties: &[RECT],
        acquired_desktop_image: &ID3D11Texture2D,
    ) -> Result<(), CaptureError> {
        if dirties.is_empty() {
            return Ok(());
        }
        // Build the scratch vertex list — every dirty rect contributes
        // NUMVERTICES (6) vertices.
        self.dirty_vertex_scratch.clear();
        let mut acquired_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { acquired_desktop_image.GetDesc(&mut acquired_desc) };
        let this_w = acquired_desc.Width as i32;
        let this_h = acquired_desc.Height as i32;
        let full_w = self.dup_output_desc.ModeDesc.Width as i32;
        let full_h = self.dup_output_desc.ModeDesc.Height as i32;
        for d in dirties {
            let verts = dxgi_compose::dirty_rect_to_vertices(*d, full_w, full_h, this_w, this_h);
            self.dirty_vertex_scratch.extend_from_slice(&verts);
        }
        let verts_needed = self.dirty_vertex_scratch.len() as u32;
        self.ensure_dirty_vertex_buffer(verts_needed)?;
        let buffer = self
            .dirty_vertex_buffer
            .as_ref()
            .expect("ensure_dirty_vertex_buffer must populate dirty_vertex_buffer");
        // Upload vertices. We allocated DEFAULT-usage buffer so we
        // must use UpdateSubresource (Map is dynamic-only).
        let mut update_box = D3D11_BOX::default();
        update_box.left = 0;
        update_box.right = verts_needed * size_of::<VERTEX>() as u32;
        update_box.top = 0;
        update_box.bottom = 1;
        update_box.front = 0;
        update_box.back = 1;
        unsafe {
            self.manager.device_context.UpdateSubresource(
                buffer,
                0,
                Some(&update_box),
                self.dirty_vertex_scratch.as_ptr() as *const _,
                0,
                0,
            );
        }
        // Bind the acquired texture as the source SRV.
        let mut srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
        srv_desc.Format = acquired_desc.Format;
        srv_desc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
        srv_desc.Anonymous.Texture2D.MostDetailedMip = acquired_desc.MipLevels - 1;
        srv_desc.Anonymous.Texture2D.MipLevels = acquired_desc.MipLevels;
        let mut srv = None;
        unsafe {
            self.manager.device.CreateShaderResourceView(
                acquired_desktop_image,
                Some(&srv_desc),
                Some(&mut srv),
            )
        }?;
        let stride = size_of::<VERTEX>() as u32;
        let offset = 0u32;
        let blend_factor = [0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32];
        unsafe {
            self.manager
                .device_context
                .OMSetBlendState(None, Some(&blend_factor), 0xFFFFFFFF);
            self.manager
                .device_context
                .OMSetRenderTargets(Some(&self.rtv), None);
            self.manager
                .device_context
                .VSSetShader(&self.manager.vertex_shader, None);
            self.manager
                .device_context
                .PSSetShader(&self.manager.pixel_shader, None);
            self.manager
                .device_context
                .PSSetShaderResources(0, Some(&[srv]));
            self.manager
                .device_context
                .PSSetSamplers(0, Some(&self.manager.sampler_linear));
            self.manager
                .device_context
                .IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.manager.device_context.IASetVertexBuffers(
                0,
                1,
                Some(&Some(buffer.clone())),
                Some(&stride),
                Some(&offset),
            );
            self.manager.device_context.Draw(verts_needed, 0);
        }
        Ok(())
    }

    pub(crate) fn get_frame<'a>(
        &mut self,
        draw_mouse: bool,
    ) -> Result<FrameAcquisitionResult<'a>, CaptureError> {
        let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut desktop_resource: Option<IDXGIResource> = None;

        let acquire_result = unsafe {
            self.dup_output
                .AcquireNextFrame(500, &mut frame_info, &mut desktop_resource)
        };

        if let Err(ref err) = acquire_result
            && err.code() == DXGI_ERROR_WAIT_TIMEOUT
        {
            // Even on a timeout the previous embedded-cursor state
            // remains observationally correct (no fresh signal to
            // contradict it) so leave `last_frame_embedded` alone.
            return Ok(FrameAcquisitionResult::NoContentChange);
        }
        acquire_result?;

        // Update embedded-cursor tracking from the *new* frame_info
        // before any early returns below. WebRTC's
        // `dxgi_output_duplicator.cc` interprets
        // `LastMouseUpdateTime != 0 && !PointerPosition.Visible` as
        // "the OS has composited the cursor into the desktop image"
        // (software cursor); when visible, the pointer is rendered
        // by a separate hardware overlay and the acquired image
        // contains no cursor pixels. This signal flips between
        // hardware and software cursor modes (e.g. after a
        // mode-change) without any DXGI error surfacing, so we must
        // recompute it every frame.
        let embedded_now = frame_contains_embedded_cursor(&frame_info);
        self.last_frame_embedded = embedded_now;

        let desktop_resource = desktop_resource.unwrap();

        // Cast immediately so we can run the size-mismatch guard
        // before any of the content_changed / cursor-only early
        // returns below. If the acquired texture's dimensions diverge
        // from `dup_output_desc.ModeDesc` it means the OS swapped to
        // a new display mode without surfacing
        // DXGI_ERROR_ACCESS_LOST; we must drop ScreenOutput and
        // rebuild against the new mode rather than keep composing
        // into a now-stale persistent RT.
        let acquired_desktop_image = desktop_resource.cast::<ID3D11Texture2D>()?;
        let mut acq_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { acquired_desktop_image.GetDesc(&mut acq_desc) };
        if acq_desc.Width != self.dup_output_desc.ModeDesc.Width
            || acq_desc.Height != self.dup_output_desc.ModeDesc.Height
        {
            log::info!(
                "[DXGI] acquired texture size {}x{} differs from dup_output_desc {}x{}; \
                 signalling rebuild",
                acq_desc.Width,
                acq_desc.Height,
                self.dup_output_desc.ModeDesc.Width,
                self.dup_output_desc.ModeDesc.Height
            );
            unsafe { self.dup_output.ReleaseFrame().ok() };
            return Ok(FrameAcquisitionResult::Rebuild);
        }

        // LastPresentTime == 0: compositor did not present a new desktop frame (cursor-only event).
        let desktop_unchanged = frame_info.LastPresentTime == 0;
        let cursor_moved = frame_info.LastMouseUpdateTime != 0
            && frame_info.LastMouseUpdateTime != self.last_mouse_update_time;
        // In RenderInFrame mode the cursor is baked into the video frame, so a cursor move with
        // static desktop still requires encoding a new frame. The
        // same holds when the OS composites the cursor itself
        // (`embedded_now`): the cursor pixel is already inside the
        // acquired image, so a cursor-only event must propagate down
        // the video pipeline or the embedded cursor stays frozen at
        // its previous location.
        let content_changed =
            !desktop_unchanged || (draw_mouse && cursor_moved) || (embedded_now && cursor_moved);

        // Capture cursor's previous-frame drawn rect *before*
        // `update_mouse_info` overwrites `self.pointer_*`. We rely on
        // `last_cursor_rect` rather than `self.pointer_visible` so the
        // hint reflects what was actually rendered last frame (which
        // may differ from `pointer_visible` if `draw_mouse` was off
        // last frame).
        let cursor_before = match self.last_cursor_rect {
            Some(rect) => dxgi_compose::CursorState {
                visible: true,
                rect,
            },
            None => dxgi_compose::CursorState::default(),
        };

        // Always update mouse tracking so SyncNative cursor sync stays accurate.
        self.update_mouse_info(&frame_info)?;

        if !content_changed {
            unsafe { self.dup_output.ReleaseFrame().ok() };
            return Ok(FrameAcquisitionResult::NoContentChange);
        }

        let frame_width = self.dup_output_desc.ModeDesc.Width;
        let frame_height = self.dup_output_desc.ModeDesc.Height;

        // RT path:
        // - full_frame_blit (default): full CopyResource of the
        //   acquired texture into the persistent RT. Avoids the
        //   cursor / resolution residue that the per-rect compose
        //   path accumulates. Skips `read_frame_metadata` because
        //   moves/dirties are unused in this branch.
        // - per-rect compose (LCXL_DXGI_DIRTY_COMPOSE opt-out): MSDN
        //   `composition_plan` — read move + dirty metadata, copy
        //   move rects, render dirty rects. The resulting
        //   moves/dirties feed `build_dirty_hint` below.
        //
        // The Option encodes "moves/dirties exist" so the
        // dirty_rects_opt branch below can match on it without a
        // separate Boolean.
        let dirty_metadata: Option<(Vec<DXGI_OUTDUPL_MOVE_RECT>, Vec<RECT>)> =
            if self.full_frame_blit {
                unsafe {
                    self.manager
                        .device_context
                        .CopyResource(&self.render_target_texture_2d, &acquired_desktop_image);
                }
                None
            } else {
                let (move_raw, dirty_raw) = if frame_info.TotalMetadataBufferSize > 0 {
                    self.read_frame_metadata(frame_info.TotalMetadataBufferSize)?
                } else {
                    (Vec::new(), Vec::new())
                };
                let moves = dxgi_compose::parse_move_rects(&move_raw);
                let dirties = dxgi_compose::parse_dirty_rects(&dirty_raw);

                // composition_plan is always applied in full;
                // fragmentation only downgrades the dirty *hint*
                // below, never the composition.
                self.copy_move_rects(&moves)?;
                self.compose_dirty_rects(&dirties, &acquired_desktop_image)?;
                Some((moves, dirties))
            };

        // --- Cursor overlay pipeline ---
        // Stage 1: snapshot the clean composed desktop into
        // `cursor_overlay_texture` so the cursor we draw next does
        // not pollute the persistent RT (cursor moves do not generate
        // dirty rects, so RT-resident cursors would leave trails).
        unsafe {
            self.manager
                .device_context
                .CopyResource(&self.cursor_overlay_texture, &self.render_target_texture_2d);
        }
        // Stage 2: draw cursor into the overlay surface. Background
        // sampling for mono/masked cursors reads from
        // `cursor_overlay_texture` itself (the clean snapshot we just
        // made), not the acquired DXGI texture (which only carries
        // valid pixels in dirty/move regions).
        let cursor_after = if draw_mouse && self.pointer_visible {
            let rect = dxgi_compose::cursor_rect_from_state(
                self.pointer_position.x,
                self.pointer_position.y,
                &self.pointer_shape_info,
                frame_width,
                frame_height,
            );
            dxgi_compose::CursorState {
                visible: true,
                rect,
            }
        } else {
            dxgi_compose::CursorState::default()
        };
        let cursor_after_shape_known = if cursor_after.visible {
            self.pointer_shape_info.Width != 0
        } else {
            true
        };
        if cursor_after.visible {
            let cursor_overlay_clone = self.cursor_overlay_texture.clone();
            let cursor_rtv_clone = self.cursor_overlay_rtv.clone();
            self.draw_mouse_into(&cursor_rtv_clone, &cursor_overlay_clone)?;
            self.last_cursor_rect = Some(cursor_after.rect);
        } else {
            self.last_cursor_rect = None;
        }

        // --- Dirty hint for downstream YUV partial-update ---
        // Full-frame-blit mode (dirty_metadata == None) forces a
        // full BGRA→YUV pass downstream. This is the safe choice on
        // software-cursor frames: `cursor_after.visible` here is
        // `draw_mouse && self.pointer_visible`, and shared_capture
        // pins SyncNative (draw_mouse=false), so build_dirty_hint
        // would see cursor_after = default() — it would not include
        // cursor move regions in the hint, and YUV partial would
        // leave the cursor's old position untouched (= ghost trail).
        // The same reasoning applies when the OS composites the
        // cursor itself (`embedded_now`): the cursor pixel is
        // baked into `acquired_desktop_image` but is not advertised
        // in the move/dirty metadata, so even the per-rect opt-out
        // path (`LCXL_DXGI_DIRTY_COMPOSE=1`) would miss the cursor's
        // previous position and accumulate ghosts. Force YUV
        // update_full whenever the cursor is embedded.
        // Cursor-aware hint optimisation is left as a follow-up.
        let dirty_rects_opt = if embedded_now {
            None
        } else {
            match dirty_metadata {
                None => None,
                Some((moves, dirties)) => dxgi_compose::build_dirty_hint(
                    &moves,
                    &dirties,
                    cursor_before,
                    cursor_after,
                    cursor_after_shape_known,
                    frame_width,
                    frame_height,
                ),
            }
        };

        // Stage 3: copy the composited frame (RT + cursor) to staging
        // for CPU readback.
        unsafe {
            self.manager
                .device_context
                .CopyResource(&self.copy_buffer_texture_2d, &self.cursor_overlay_texture);
        };
        let mut locked_rect = DXGI_MAPPED_RECT::default();
        let frame_buffer = unsafe {
            self.copy_buffer_surface
                .Map(&mut locked_rect, DXGI_MAP_READ)?;
            core::slice::from_raw_parts(
                locked_rect.pBits,
                locked_rect.Pitch as usize * self.dup_output_desc.ModeDesc.Height as usize,
            )
        };

        Ok(FrameAcquisitionResult::ContentFrame(SceenFrame {
            height: self.dup_output_desc.ModeDesc.Height,
            width: self.dup_output_desc.ModeDesc.Width,
            pitch: locked_rect.Pitch as u32,
            frame_buffer,
            copy_buffer_surface: self.copy_buffer_surface.clone(),
            dup_output: self.dup_output.clone(),
            dirty_rects: dirty_rects_opt,
        }))
    }

    /// Create render target texture
    pub fn create_render_target_texture(
        device: &ID3D11Device,
        desktop_width: u32,
        desktop_height: u32,
    ) -> Result<ID3D11Texture2D, CaptureError> {
        // Create render target texture
        let mut render_target_texture_2d_desc: D3D11_TEXTURE2D_DESC = unsafe { std::mem::zeroed() };

        render_target_texture_2d_desc.Width = desktop_width;
        render_target_texture_2d_desc.Height = desktop_height;
        render_target_texture_2d_desc.MipLevels = 1;
        render_target_texture_2d_desc.ArraySize = 1;
        //The format must be DXGI_FORMAT_B8G8R8A8_UNORM, see https://learn.microsoft.com/zh-cn/windows/win32/direct3ddxgi/desktop-dup-api#updating-the-desktop-image-data
        render_target_texture_2d_desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        render_target_texture_2d_desc.SampleDesc.Count = 1;
        render_target_texture_2d_desc.SampleDesc.Quality = 0;
        render_target_texture_2d_desc.Usage = D3D11_USAGE_DEFAULT;
        render_target_texture_2d_desc.BindFlags =
            D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32;
        render_target_texture_2d_desc.CPUAccessFlags = 0;
        render_target_texture_2d_desc.MiscFlags = 0;
        let mut render_target_texture_2d = None;

        unsafe {
            device.CreateTexture2D(
                &render_target_texture_2d_desc,
                None,
                Some(&mut render_target_texture_2d),
            )
        }?;
        let render_target_texture_2d = render_target_texture_2d.unwrap();
        Ok(render_target_texture_2d)
    }

    pub fn update_mouse_info(
        &mut self,
        frame_info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> Result<(), CaptureError> {
        let mut update_position = true;
        if frame_info.LastMouseUpdateTime == 0 {
            update_position = false;
        }
        if self.last_mouse_update_time > frame_info.LastMouseUpdateTime {
            update_position = false;
        }
        if update_position {
            self.last_mouse_update_time = frame_info.LastMouseUpdateTime;
            self.pointer_position.x = frame_info.PointerPosition.Position.x;
            self.pointer_position.y = frame_info.PointerPosition.Position.y;
            self.pointer_visible = frame_info.PointerPosition.Visible.as_bool();

            if self.pointer_visible {
                // check if the mouse shape has changed
                if frame_info.PointerShapeBufferSize > 0 {
                    self.pointer_shape_buffer =
                        vec![0u8; frame_info.PointerShapeBufferSize as usize];
                    let mut buffer_size_required: u32 = 0;
                    let result = unsafe {
                        self.dup_output.GetFramePointerShape(
                            frame_info.PointerShapeBufferSize,
                            self.pointer_shape_buffer.as_mut_ptr() as *mut _,
                            &mut buffer_size_required,
                            &mut self.pointer_shape_info,
                        )
                    };
                    if let Err(error) = result {
                        log::error!("Failed to get frame pointer shape: {}", error);
                        self.pointer_shape_buffer = vec![];
                        return Err(CaptureError::from(error));
                    }
                    log::trace!("Pointer shape info: {:?}", self.pointer_shape_info);
                }
            }
        }
        Ok(())
    }

    /// Draw mouse cursor on the screen
    pub fn draw_mouse_into(
        &mut self,
        target_rtv: &[Option<ID3D11RenderTargetView>; 1],
        background_texture: &ID3D11Texture2D,
    ) -> Result<(), CaptureError> {
        if !self.pointer_visible {
            log::trace!("Pointer is not visible, skipping drawing pointer shape.");
            // If the pointer is not visible, we don't need to draw anything. Just return.
            return Ok(());
        }

        let is_mono = self.pointer_shape_info.Type == POINTER_SHAPE_TYPE_MONOCHROME;
        // Render target dimensions — equal to the persistent RT, not
        // the acquired texture (those have the same dimensions today
        // but conceptually we are drawing onto the composed surface).
        let mut full_desc: D3D11_TEXTURE2D_DESC = D3D11_TEXTURE2D_DESC::default();
        unsafe { background_texture.GetDesc(&mut full_desc) };
        let desktop_width = full_desc.Width as i32;
        let desktop_height = full_desc.Height as i32;

        // Center of desktop dimensions
        let center_x = desktop_width / 2;
        let center_y = desktop_height / 2;
        // Pointer position
        let given_left = self.pointer_position.x;
        let given_top = self.pointer_position.y;

        // Display dimensions of the cursor — for monochrome cursors,
        // `shape_info.Height` is the AND mask + XOR mask combined
        // height, so the actual display height is half.
        let (cursor_w, cursor_h) = dxgi_compose::cursor_display_size(&self.pointer_shape_info);
        let cursor_w_i = cursor_w as i32;
        let cursor_h_i = cursor_h as i32;

        // Figure out if any adjustment is needed for out of bound positions
        let ptr_width = if given_left < 0 {
            given_left + cursor_w_i
        } else if (given_left + cursor_w_i) > desktop_width {
            desktop_width - given_left
        } else {
            cursor_w_i
        };

        let ptr_height = if given_top < 0 {
            given_top + cursor_h_i
        } else if (given_top + cursor_h_i) > desktop_height {
            desktop_height - given_top
        } else {
            cursor_h_i
        };

        let ptr_left = if given_left < 0 { 0 } else { given_left };
        let ptr_top = if given_top < 0 { 0 } else { given_top };
        log::trace!(
            "desktop_width: {desktop_width}, 
            desktop_height: {desktop_height}, 
            given_left: {given_left}, 
            given_top: {given_top},
            ptr_width: {ptr_width}, 
            ptr_height: {ptr_height}, 
            ptr_left: {ptr_left}, 
            ptr_top: {ptr_top},
            is_mono: {is_mono}, 
            "
        );
        // New mouseshape buffer
        let mut init_buffer = vec![0u8; (ptr_width * ptr_height * BPP) as usize];
        match self.pointer_shape_info.Type {
            //DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME | DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR
            POINTER_SHAPE_TYPE_MONOCHROME | POINTER_SHAPE_TYPE_MASKED_COLOR => {
                self.process_mono_and_masked_pointer(
                    &mut init_buffer,
                    background_texture,
                    is_mono,
                    ptr_width,
                    ptr_height,
                    ptr_left,
                    ptr_top,
                    given_left,
                    given_top,
                )?;
            }
            //DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR
            POINTER_SHAPE_TYPE_COLOR => {}
            _ => {
                log::warn!(
                    "Unsupported pointer shape type: {}",
                    self.pointer_shape_info.Type
                );
            }
        }

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        desc.SampleDesc.Count = 1;
        desc.SampleDesc.Quality = 0;
        desc.Usage = D3D11_USAGE_DEFAULT;
        desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
        desc.CPUAccessFlags = 0;
        desc.MiscFlags = 0;
        // Set texture properties
        desc.Width = ptr_width as u32;
        desc.Height = ptr_height as u32;

        // Set up init data
        let mut init_data = D3D11_SUBRESOURCE_DATA::default();
        init_data.pSysMem = if self.pointer_shape_info.Type == POINTER_SHAPE_TYPE_COLOR {
            //log::trace!("Use pointer shape buffer: {:?}", self.pointer_shape_buffer);
            self.pointer_shape_buffer.as_ptr() as *const _
        } else {
            init_buffer.as_ptr() as *const _
        };
        init_data.SysMemPitch = if self.pointer_shape_info.Type == POINTER_SHAPE_TYPE_COLOR {
            self.pointer_shape_info.Pitch
        } else {
            (ptr_width * BPP) as u32
        };
        init_data.SysMemSlicePitch = 0;

        // Create mouseshape as texture
        let mut mouse_tex = None;
        unsafe {
            self.manager
                .device
                .CreateTexture2D(&desc, Some(&init_data), Some(&mut mouse_tex))
        }?;
        let mouse_tex = mouse_tex.unwrap();

        // Set shader resource properties
        let mut s_desc = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
        s_desc.Format = desc.Format;
        s_desc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
        s_desc.Anonymous.Texture2D.MostDetailedMip = desc.MipLevels - 1;
        s_desc.Anonymous.Texture2D.MipLevels = desc.MipLevels;

        // Position will be changed based on mouse position
        let mut vertices = VERTICES;

        // VERTEX creation
        vertices[0].pos.x = (ptr_left - center_x) as f32 / center_x as f32;
        vertices[0].pos.y = -(((ptr_top + ptr_height) - center_y) as f32) / center_y as f32;
        vertices[1].pos.x = (ptr_left - center_x) as f32 / center_x as f32;
        vertices[1].pos.y = -((ptr_top - center_y) as f32) / center_y as f32;
        vertices[2].pos.x = ((ptr_left + ptr_width) - center_x) as f32 / center_x as f32;
        vertices[2].pos.y = -(((ptr_top + ptr_height) - center_y) as f32) / center_y as f32;
        vertices[3].pos.x = vertices[2].pos.x;
        vertices[3].pos.y = vertices[2].pos.y;
        vertices[4].pos.x = vertices[1].pos.x;
        vertices[4].pos.y = vertices[1].pos.y;
        vertices[5].pos.x = ((ptr_left + ptr_width) - center_x) as f32 / center_x as f32;
        vertices[5].pos.y = -((ptr_top - center_y) as f32) / center_y as f32;

        let mut shader_res = None;
        // Create shader resource from texture
        unsafe {
            self.manager.device.CreateShaderResourceView(
                &mouse_tex,
                Some(&s_desc),
                Some(&mut shader_res),
            )
        }?;

        let mut b_desc = D3D11_BUFFER_DESC::default();

        b_desc.Usage = D3D11_USAGE_DEFAULT;
        b_desc.ByteWidth = size_of::<VERTEX>() as u32 * NUMVERTICES;
        b_desc.BindFlags = D3D11_BIND_VERTEX_BUFFER.0 as u32;
        b_desc.CPUAccessFlags = 0;

        let mut init_data = D3D11_SUBRESOURCE_DATA::default();
        init_data.pSysMem = vertices.as_ptr() as *const _;

        // Create vertex buffer
        let mut vertex_buffer_mouse = None;
        unsafe {
            self.manager.device.CreateBuffer(
                &b_desc,
                Some(&init_data),
                Some(&mut vertex_buffer_mouse),
            )
        }?;
        // Set resources
        let blend_factor = [0.0f32, 0.0f32, 0.0f32, 0.0f32];
        let stride = size_of::<VERTEX>() as u32;
        let offset = 0;
        unsafe {
            self.manager.device_context.IASetVertexBuffers(
                0,
                1,
                Some(&vertex_buffer_mouse),
                Some(&stride),
                Some(&offset),
            );
            self.manager.device_context.OMSetBlendState(
                &self.manager.blend_state,
                Some(&blend_factor),
                0xFFFFFFFF,
            );
            self.manager
                .device_context
                .OMSetRenderTargets(Some(target_rtv), None);
            self.manager
                .device_context
                .VSSetShader(&self.manager.vertex_shader, None);
            self.manager
                .device_context
                .PSSetShader(&self.manager.pixel_shader, None);
            self.manager
                .device_context
                .PSSetShaderResources(0, Some(&[shader_res]));
            self.manager
                .device_context
                .PSSetSamplers(0, Some(&self.manager.sampler_linear));

            // Draw
            self.manager.device_context.Draw(NUMVERTICES, 0);
        }
        Ok(())
    }

    fn process_mono_and_masked_pointer(
        &mut self,
        init_buffer: &mut Vec<u8>,
        background_texture: &ID3D11Texture2D,
        is_mono: bool,
        ptr_width: i32,
        ptr_height: i32,
        ptr_left: i32,
        ptr_top: i32,
        given_left: i32,
        given_top: i32,
    ) -> Result<(), CaptureError> {
        if self.pointer_shape_info.Type != POINTER_SHAPE_TYPE_MONOCHROME
            && self.pointer_shape_info.Type != POINTER_SHAPE_TYPE_MASKED_COLOR
        {
            panic!("Invalid pointer shape type");
        }

        // Staging buffer/texture
        let mut copy_buffer_desc = D3D11_TEXTURE2D_DESC::default();
        copy_buffer_desc.Width = ptr_width as u32;
        copy_buffer_desc.Height = ptr_height as u32;
        copy_buffer_desc.MipLevels = 1;
        copy_buffer_desc.ArraySize = 1;
        copy_buffer_desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        copy_buffer_desc.SampleDesc.Count = 1;
        copy_buffer_desc.SampleDesc.Quality = 0;
        copy_buffer_desc.Usage = D3D11_USAGE_STAGING;
        copy_buffer_desc.BindFlags = 0;
        copy_buffer_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        copy_buffer_desc.MiscFlags = 0;

        let mut copy_buffer = None;
        unsafe {
            self.manager
                .device
                .CreateTexture2D(&copy_buffer_desc, None, Some(&mut copy_buffer))
        }?;
        let copy_buffer = copy_buffer.unwrap();
        // Copy needed part of desktop image
        let mut d3d11_box = D3D11_BOX::default();
        d3d11_box.left = ptr_left as u32;
        d3d11_box.top = ptr_top as u32;
        d3d11_box.right = (ptr_left + ptr_width) as u32;
        d3d11_box.bottom = (ptr_top + ptr_height) as u32;
        d3d11_box.front = 0;
        d3d11_box.back = 1;

        unsafe {
            self.manager.device_context.CopySubresourceRegion(
                &copy_buffer,
                0,
                0,
                0,
                0,
                background_texture,
                0,
                Some(&d3d11_box),
            )
        };
        // QI for IDXGISurface
        let copy_resource = copy_buffer.cast::<IDXGISurface>()?;
        // Map pixels
        let mut mapped_surface = DXGI_MAPPED_RECT::default();
        unsafe { copy_resource.Map(&mut mapped_surface, DXGI_MAP_READ) }?;

        // New mouseshape buffer
        let init_buffer_32 = unsafe {
            core::slice::from_raw_parts_mut(
                init_buffer.as_mut_ptr() as *mut u32,
                init_buffer.len() / size_of::<u32>(),
            )
        };

        let desktop_32 = mapped_surface.pBits as *const u32;
        let desktop_pitch_in_pixels = (mapped_surface.Pitch / size_of::<u32>() as i32) as u32;

        // What to skip (pixel offset)
        let skip_x = if given_left < 0 {
            -given_left as u32
        } else {
            0
        };
        let skip_y = if given_top < 0 { -given_top as u32 } else { 0 };

        if is_mono {
            for row in 0..ptr_height {
                // Set mask
                let mut mask = 0x80u8;
                mask >>= skip_x % 8;
                for col in 0..ptr_width {
                    // Get masks using appropriate offsets
                    let and_mask = self.pointer_shape_buffer[((col + skip_x as i32) / 8
                        + (row + skip_y as i32) * (self.pointer_shape_info.Pitch as i32))
                        as usize]
                        & mask;
                    let xor_mask = self.pointer_shape_buffer[((col + skip_x as i32) / 8
                        + (row + skip_y as i32 + (self.pointer_shape_info.Height as i32 / 2))
                            * (self.pointer_shape_info.Pitch as i32))
                        as usize]
                        & mask;
                    let and_mask_32 = if and_mask != 0 {
                        0xFFFFFFFF_u32
                    } else {
                        0xFF000000
                    };
                    let xor_mask_32 = if xor_mask != 0 {
                        0x00FFFFFF_u32
                    } else {
                        0x00000000
                    };

                    // Set new pixel
                    init_buffer_32[(row * ptr_width + col) as usize] = (unsafe {
                        *desktop_32
                            .wrapping_add((row * desktop_pitch_in_pixels as i32 + col) as usize)
                    } & and_mask_32)
                        ^ xor_mask_32;

                    // Adjust mask
                    if mask == 0x01 {
                        mask = 0x80;
                    } else {
                        mask >>= 1;
                    }
                }
            }
        } else {
            let buffer_32 = unsafe {
                core::slice::from_raw_parts_mut(
                    self.pointer_shape_buffer.as_mut_ptr() as *mut u32,
                    self.pointer_shape_buffer.len() / size_of::<u32>(),
                )
            };

            // Iterate through pixels
            for row in 0..ptr_height {
                for col in 0..ptr_width {
                    // Set up mask
                    let mask_val = 0xFF000000
                        & buffer_32[(col
                            + skip_x as i32
                            + (row + skip_y as i32)
                                * (self.pointer_shape_info.Pitch as i32 / size_of::<u32>() as i32))
                            as usize];
                    if mask_val != 0 {
                        // Mask was 0xFF
                        init_buffer_32[(row * ptr_width + col) as usize] = (unsafe {
                            *desktop_32
                                .wrapping_add((row * desktop_pitch_in_pixels as i32 + col) as usize)
                        } ^ buffer_32[(col
                            + skip_x as i32
                            + (row + skip_y as i32)
                                * (self.pointer_shape_info.Pitch as i32 / size_of::<u32>() as i32))
                            as usize])
                            | 0xFF000000;
                    } else {
                        // Mask was 0x00
                        init_buffer_32[(row * ptr_width + col) as usize] = buffer_32[(col
                            + skip_x as i32
                            + (row + skip_y as i32)
                                * (self.pointer_shape_info.Pitch as i32 / size_of::<u32>() as i32))
                            as usize]
                            | 0xFF000000;
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct SceenFrame<'a> {
    pub height: u32,
    pub width: u32,
    pub pitch: u32,
    pub frame_buffer: &'a [u8],
    pub copy_buffer_surface: IDXGISurface,
    pub dup_output: IDXGIOutputDuplication,
    /// None = full update required; Some(rects) = only these regions changed
    pub dirty_rects: Option<Vec<DirtyRect>>,
}

impl Drop for SceenFrame<'_> {
    fn drop(&mut self) {
        unsafe {
            let ummap_result = self.copy_buffer_surface.Unmap();
            if let Err(e) = ummap_result {
                log::warn!(
                    "Failed to unmap surface: code: {}, message: {}",
                    e.code(),
                    e.message()
                );
            }

            let release_result = self.dup_output.ReleaseFrame();
            if let Err(e) = release_result {
                log::warn!(
                    "Failed to release frame: code: {}, message: {}",
                    e.code(),
                    e.message()
                );
            }
        }
    }
}

impl ImageInfo for SceenFrame<'_> {
    fn get_type(&self) -> ImageType {
        ImageType::BGRA
    }

    fn get_data(&self) -> &[u8] {
        self.frame_buffer
    }

    fn get_width(&self) -> u32 {
        self.width
    }

    fn get_height(&self) -> u32 {
        self.height
    }

    fn get_stride(&self) -> u32 {
        self.pitch
    }

    fn get_dirty_rects(&self) -> Option<&[DirtyRect]> {
        self.dirty_rects.as_deref()
    }
}

pub struct DxgiImageOutputEnumerator {}

impl Default for DxgiImageOutputEnumerator {
    fn default() -> Self {
        Self::new()
    }
}

impl DxgiImageOutputEnumerator {
    pub fn new() -> Self {
        DxgiImageOutputEnumerator {}
    }
}

impl ImageOutputEnumerator for DxgiImageOutputEnumerator {
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        // Cross-adapter enumeration: see `enumerate_all_outputs`. The
        // flat order is the dropdown order: default hardware adapter
        // is placed first so
        // single-GPU users see the same indices as before.
        let entries = enumerate_all_outputs()?;
        log::info!(
            "DxgiImageOutputEnumerator: enumerated {} output(s) across all adapters",
            entries.len()
        );
        Ok(entries
            .iter()
            .map(|e| from_dxgi_output_desc(&e.desc))
            .collect())
    }
}

pub struct DxgiImageCapture {
    pub manager: Arc<ScreenRecordManager>,
    /// GDI device name (`\\.\DISPLAYn`) of the chosen output. The
    /// equivalent of the legacy `output_index` for diagnostics and for
    /// the shared-capture registry's `effective_key` (see
    /// `shared_capture::get_or_initialize`).
    pub device_name: String,
    /// Position of the chosen adapter in the flat ordering returned by
    /// [`enumerate_all_outputs`]. Used for diagnostics only.
    adapter_index: u32,
    /// `EnumOutputs` index *within* the chosen adapter — what
    /// `manager.dxgi_adapter.EnumOutputs()` and
    /// `ScreenOutput::new(manager, idx)` actually want. Recomputed at
    /// `new` time from the chosen `EnumeratedOutput`.
    local_output_index: u32,
    pub screen_output: Option<ScreenOutput>,
    last_cursor_fingerprint: Option<DxgiCursorFingerprint>,
}

impl DxgiImageCapture {
    pub fn new(settings: &DeskSettings) -> Result<Self, CaptureError> {
        let entries = enumerate_all_outputs()?;
        let chosen = select_output_by_name(&entries, &settings.video_device_name)?;
        let chosen_adapter_index = chosen.adapter_index;
        let chosen_local_index = chosen.local_output_index;
        let chosen_adapter_name = adapter_name_from_desc(&chosen.adapter_desc);
        let chosen_device_name = output_device_name(&chosen.desc);
        log::info!(
            "DxgiImageCapture::new: device_name={:?} → adapter[{}]='{}' local_output_index={}",
            chosen_device_name,
            chosen_adapter_index,
            chosen_adapter_name,
            chosen_local_index
        );
        let manager = ScreenRecordManager::new_with_adapter(settings, &chosen.adapter)?;
        let screen_output = Some(ScreenOutput::new(manager.clone(), chosen_local_index)?);
        Ok(DxgiImageCapture {
            manager,
            device_name: chosen_device_name,
            adapter_index: chosen_adapter_index,
            local_output_index: chosen_local_index,
            screen_output,
            last_cursor_fingerprint: None,
        })
    }

    fn capture_cursor_update(
        screen_output: &ScreenOutput,
    ) -> Result<Option<(DxgiCursorFingerprint, CursorSyncData)>, CaptureError> {
        // Branch 1: OS has composited the cursor into the desktop
        // frame (software-cursor path). Tell the front-end to hide
        // its CSS cursor and trust the video stream's baked-in
        // cursor. The Embedded fingerprint is distinct from both
        // Hidden and Shape{...} so toggling between hardware-cursor
        // and software-cursor modes always emits a fresh payload
        // (PartialEq on the enum drives the dedup in the caller).
        if screen_output.last_frame_embedded {
            let mut full_desc =
                windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
            unsafe { screen_output.copy_buffer_texture_2d.GetDesc(&mut full_desc) };
            return Ok(Some((
                DxgiCursorFingerprint::Embedded,
                CursorSyncData {
                    visible: false,
                    embedded: true,
                    screen_width: full_desc.Width,
                    screen_height: full_desc.Height,
                    ..Default::default()
                },
            )));
        }

        if !screen_output.pointer_visible {
            return Ok(Some((
                DxgiCursorFingerprint::Hidden,
                CursorSyncData {
                    visible: false,
                    ..Default::default()
                },
            )));
        }

        if screen_output.pointer_shape_buffer.is_empty() {
            return Ok(None);
        }

        let info = &screen_output.pointer_shape_info;
        let mut rgba_buffer = Vec::new();
        let width = info.Width;
        let height = if info.Type == POINTER_SHAPE_TYPE_MONOCHROME {
            info.Height / 2
        } else {
            info.Height
        };

        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        screen_output.pointer_shape_buffer.hash(&mut hasher);
        let shape_id = hasher.finish();

        if info.Type == POINTER_SHAPE_TYPE_COLOR || info.Type == POINTER_SHAPE_TYPE_MASKED_COLOR {
            let src = &screen_output.pointer_shape_buffer;
            for y in 0..height {
                let row_start = (y * info.Pitch) as usize;
                for x in 0..width {
                    let pixel_start = row_start + (x * 4) as usize;
                    if pixel_start + 3 < src.len() {
                        let b = src[pixel_start];
                        let g = src[pixel_start + 1];
                        let r = src[pixel_start + 2];
                        let a = src[pixel_start + 3];
                        if info.Type == POINTER_SHAPE_TYPE_MASKED_COLOR {
                            let a_val = if a != 0 { 255 } else { 0 };
                            rgba_buffer.extend_from_slice(&[r, g, b, a_val]);
                        } else {
                            rgba_buffer.extend_from_slice(&[r, g, b, a]);
                        }
                    } else {
                        rgba_buffer.extend_from_slice(&[0, 0, 0, 0]);
                    }
                }
            }
        } else {
            let src = &screen_output.pointer_shape_buffer;
            let pitch = info.Pitch as usize;
            for y in 0..height {
                let and_row = y as usize * pitch;
                let xor_row = (y + height) as usize * pitch;
                for x in 0..width {
                    let bit_offset = x % 8;
                    let byte_offset = (x / 8) as usize;
                    let and_byte = src.get(and_row + byte_offset).copied().unwrap_or(0);
                    let xor_byte = src.get(xor_row + byte_offset).copied().unwrap_or(0);
                    let mask = 0x80 >> bit_offset;
                    let and_bit = (and_byte & mask) != 0;
                    let xor_bit = (xor_byte & mask) != 0;
                    let (r, g, b, a) = match (and_bit, xor_bit) {
                        (true, false) => (0, 0, 0, 0),
                        (false, false) => (0, 0, 0, 255),
                        (false, true) => (255, 255, 255, 255),
                        (true, true) => (0, 0, 0, 255),
                    };
                    rgba_buffer.extend_from_slice(&[r, g, b, a]);
                }
            }
        }

        use image::{ImageBuffer, Rgba};
        use std::io::Cursor;
        let img = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, rgba_buffer)
            .unwrap_or_else(|| ImageBuffer::new(width, height));
        let mut png_data = Cursor::new(Vec::new());
        img.write_to(&mut png_data, image::ImageFormat::Png)
            .map_err(|e| {
                CaptureError::custom_error::<()>(DeskErrorCode::SYSTEM_ERROR, &e.to_string())
                    .unwrap_err()
            })?;
        use base64::Engine;
        let base64_png = base64::engine::general_purpose::STANDARD.encode(png_data.into_inner());

        let mut full_desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        unsafe { screen_output.copy_buffer_texture_2d.GetDesc(&mut full_desc) };

        Ok(Some((
            DxgiCursorFingerprint::Shape {
                id: shape_id,
                screen_width: full_desc.Width,
                screen_height: full_desc.Height,
            },
            CursorSyncData {
                base64_png,
                hotspot_x: info.HotSpot.x,
                hotspot_y: info.HotSpot.y,
                visible: true,
                shape_id,
                screen_width: full_desc.Width,
                screen_height: full_desc.Height,
                embedded: false,
            },
        )))
    }

    /// Reset the cursor fingerprint cache so the next capture pass
    /// re-emits a full `CursorSyncData`. Called on the resource
    /// rebuild paths (DXGI_ERROR_ACCESS_LOST, FrameAcquisitionResult::Rebuild)
    /// as a defensive backstop in case the new ScreenOutput's first
    /// fingerprint happens to coincide with the stale one (e.g. same
    /// shape_id + same dimensions). The size-aware fingerprint
    /// already covers the common case where dimensions change.
    pub fn reset_cursor_cache(&mut self) {
        self.last_cursor_fingerprint = None;
    }
}

impl ImageCapture for DxgiImageCapture {
    fn capture(&mut self, request: CaptureRequest) -> Result<CaptureResult, CaptureError> {
        let draw_mouse = matches!(request.cursor_mode, CursorCaptureMode::RenderInFrame);
        log::trace!("Start to get screen output frame");
        if self.screen_output.is_none() {
            // Use the local (per-adapter) index, not the flat
            // position — `manager.dxgi_adapter` is the adapter we
            // picked in `new()`, so EnumOutputs there only accepts
            // indices within that adapter.
            log::debug!(
                "ScreenOutput rebuild on adapter_index={} local_output_index={} device_name={:?}",
                self.adapter_index,
                self.local_output_index,
                self.device_name
            );
            self.screen_output = Some(ScreenOutput::new(
                self.manager.clone(),
                self.local_output_index,
            )?);
        }
        let screen_output = self.screen_output.as_mut().unwrap();
        let acq_result = match screen_output.get_frame(draw_mouse) {
            Ok(r) => r,
            Err(error) => {
                if let CaptureError::WindowsResultError(bt, err) = error {
                    if err.code() == DXGI_ERROR_ACCESS_LOST || err.code() == DXGI_ERROR_INVALID_CALL
                    {
                        self.screen_output = None;
                        // Defensive: a brand-new ScreenOutput might
                        // happen to land on the same fingerprint as
                        // the previous one (same cursor shape, same
                        // dimensions); explicit reset guarantees the
                        // next frame re-emits cursor metadata.
                        self.reset_cursor_cache();
                        return CaptureError::custom_error(
                            DeskErrorCode::ACTION_NEED_RETRY,
                            &format!("capture frame is lost, will retry, error={}", err),
                        );
                    } else {
                        if err.code() == DXGI_ERROR_DEVICE_REMOVED {
                            let removed_reason =
                                unsafe { self.manager.device.GetDeviceRemovedReason() };
                            log::error!("Device removed reason: {:?}", removed_reason);
                            return Err(CaptureError::WindowsResultError(
                                Backtrace::disabled(),
                                err,
                            ));
                        }
                        return Err(CaptureError::WindowsResultError(bt, err));
                    }
                } else {
                    return Err(error);
                }
            }
        };

        match acq_result {
            FrameAcquisitionResult::NoContentChange => Ok(CaptureResult {
                image: Box::new(EmptyImageInfo),
                cursor_update: None,
                content_changed: false,
                dirty_rects: Some(vec![]),
            }),
            FrameAcquisitionResult::Rebuild => {
                // Resolution change detected inside get_frame —
                // discard ScreenOutput so the next capture() tick
                // builds a fresh one against the new mode. Surface
                // as ACTION_NEED_RETRY so shared_capture's 16ms
                // back-off bridges the gap (same pattern as
                // DXGI_ERROR_ACCESS_LOST below).
                self.screen_output = None;
                // Defensive: see ACCESS_LOST branch.
                self.reset_cursor_cache();
                CaptureError::custom_error(
                    DeskErrorCode::ACTION_NEED_RETRY,
                    "[DXGI] resolution changed mid-session; ScreenOutput rebuild scheduled",
                )
            }
            FrameAcquisitionResult::ContentFrame(screen_frame) => {
                let mut cursor_update = None;
                if matches!(request.cursor_mode, CursorCaptureMode::SyncNative) {
                    if let Some(screen_output) = self.screen_output.as_ref() {
                        match Self::capture_cursor_update(screen_output) {
                            Ok(Some((fingerprint, data))) => {
                                if self.last_cursor_fingerprint != Some(fingerprint) {
                                    self.last_cursor_fingerprint = Some(fingerprint);
                                    cursor_update = Some(data);
                                }
                            }
                            Ok(None) => {}
                            Err(err) => {
                                log::warn!(
                                    "Failed to capture cursor update in DXGI backend: {}",
                                    err
                                );
                            }
                        }
                    }
                } else {
                    self.last_cursor_fingerprint = None;
                }

                // Propagate the dirty_hint built inside `get_frame`
                // so downstream YUV partial-update sees the actual
                // changed regions. Pre-fix this was hardcoded to
                // `None`, forcing every frame through full conversion
                // and masking the underlying RT-corruption bug.
                let dirty_rects = screen_frame.dirty_rects.clone();
                Ok(CaptureResult {
                    image: Box::new(screen_frame),
                    cursor_update,
                    content_changed: true,
                    dirty_rects,
                })
            }
        }
    }

    fn supports_cursor_sync(&self) -> bool {
        true
    }

    fn get_capture_type(&self) -> ImageCaptureType {
        ImageCaptureType::DXGI
    }

    fn get_current_output(&self) -> Result<DisplayInfo, CaptureError> {
        // Local index, not flat — see field docs on
        // `DxgiImageCapture::local_output_index`.
        let output = unsafe {
            self.manager
                .dxgi_adapter
                .EnumOutputs(self.local_output_index)?
        };
        let output_desc: DXGI_OUTPUT_DESC = unsafe { output.GetDesc() }?;
        Ok(from_dxgi_output_desc(&output_desc))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::thread;

    use crate::model::image_capture::{CaptureRequest, CursorCaptureMode};
    use desk_utils::logs::init_logs;
    use log::LevelFilter;
    use std::sync::{Barrier, Once};
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::System::StationsAndDesktops::{
        CloseWindowStation, CreateDesktopW, EnumDesktopsW, EnumWindowStationsW,
        GetProcessWindowStation, GetThreadDesktop, HWINSTA, OpenDesktopW, OpenWindowStationW,
        SwitchDesktop,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Shell::IsUserAnAdmin;
    use windows::Win32::UI::WindowsAndMessaging::{MB_OK, MessageBoxW};
    use windows_core::w;
    use yuv::bgra_to_rgba;

    use super::*;

    static INIT: Once = Once::new();

    // -----------------------------------------------------------------
    // Pure-function tests (cross-adapter enumeration helpers).
    // No DXGI fixtures needed.
    // -----------------------------------------------------------------

    /// Smoke test for `FrameAcquisitionResult::Rebuild`: the new
    /// variant exists, is constructible, and is reachable via
    /// exhaustive match. Compiler-enforced exhaustiveness on
    /// downstream `match acq_result { ... }` is the real guarantee;
    /// this test exists so a future reader who deletes the variant
    /// also has to update an explicit assertion.
    #[test]
    fn screen_output_rebuild_variant_can_be_matched() {
        let r: FrameAcquisitionResult<'_> = FrameAcquisitionResult::Rebuild;
        let tag = match r {
            FrameAcquisitionResult::Rebuild => "rebuild",
            FrameAcquisitionResult::NoContentChange => "nochange",
            FrameAcquisitionResult::ContentFrame(_) => "content",
        };
        assert_eq!(tag, "rebuild");
    }

    // -----------------------------------------------------------------
    // Cursor fingerprint (M1) — size-aware identity.
    // -----------------------------------------------------------------

    /// Two `Shape` fingerprints with the same `id` but different
    /// `screen_width` are not equal. This guarantees a mid-session
    /// resolution change re-emits cursor metadata even when the
    /// cursor pixel hash is unchanged — otherwise the front-end's
    /// stale `screen_width` would mis-scale the cursor sprite.
    #[test]
    fn dxgi_fingerprint_differs_on_screen_width_change() {
        let a = DxgiCursorFingerprint::Shape {
            id: 0xabcd,
            screen_width: 1920,
            screen_height: 1080,
        };
        let b = DxgiCursorFingerprint::Shape {
            id: 0xabcd,
            screen_width: 2560,
            screen_height: 1080,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn dxgi_fingerprint_differs_on_screen_height_change() {
        let a = DxgiCursorFingerprint::Shape {
            id: 0xabcd,
            screen_width: 1920,
            screen_height: 1080,
        };
        let b = DxgiCursorFingerprint::Shape {
            id: 0xabcd,
            screen_width: 1920,
            screen_height: 1440,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn dxgi_fingerprint_equal_when_all_fields_match() {
        let a = DxgiCursorFingerprint::Shape {
            id: 0xabcd,
            screen_width: 1920,
            screen_height: 1080,
        };
        let b = DxgiCursorFingerprint::Shape {
            id: 0xabcd,
            screen_width: 1920,
            screen_height: 1080,
        };
        assert_eq!(a, b);
    }

    /// `Embedded` is the third state. It must compare unequal to
    /// both `Hidden` and any `Shape` so transitions between
    /// hardware-cursor and software-cursor modes always emit a
    /// fresh `CursorSyncData` (the front-end uses the new payload
    /// to toggle its local CSS cursor visibility).
    #[test]
    fn dxgi_fingerprint_embedded_differs_from_hidden_and_shape() {
        let embedded = DxgiCursorFingerprint::Embedded;
        let hidden = DxgiCursorFingerprint::Hidden;
        let shape = DxgiCursorFingerprint::Shape {
            id: 1,
            screen_width: 1920,
            screen_height: 1080,
        };
        assert_ne!(embedded, hidden);
        assert_ne!(embedded, shape);
        assert_ne!(hidden, shape);
    }

    // -----------------------------------------------------------------
    // Embedded-cursor detection (M3) — WebRTC heuristic.
    // -----------------------------------------------------------------

    fn frame_info_for_embedded_test(
        last_mouse_update_time: i64,
        pointer_visible: bool,
    ) -> DXGI_OUTDUPL_FRAME_INFO {
        let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        frame_info.LastMouseUpdateTime = last_mouse_update_time;
        frame_info.PointerPosition.Visible =
            windows_core::BOOL(if pointer_visible { 1 } else { 0 });
        frame_info
    }

    /// Software-cursor frame: the OS reports a fresh pointer-position
    /// update (LastMouseUpdateTime != 0) but tags it not-visible
    /// (i.e. there's no separate hardware pointer plane to render),
    /// meaning the cursor pixel is already composited into the
    /// desktop image.
    #[test]
    fn frame_contains_embedded_cursor_true_when_invisible_and_mouse_update() {
        let f = frame_info_for_embedded_test(0x1234, false);
        assert!(frame_contains_embedded_cursor(&f));
    }

    /// Hardware-cursor frame: pointer is visible as a separate
    /// overlay plane, so it is not part of the acquired desktop
    /// image. The fact that LastMouseUpdateTime is non-zero only
    /// means the OS has fresh pointer position info to deliver.
    #[test]
    fn frame_contains_embedded_cursor_false_when_visible_with_update() {
        let f = frame_info_for_embedded_test(0x1234, true);
        assert!(!frame_contains_embedded_cursor(&f));
    }

    /// No pointer-position update this frame — the duplication API
    /// gives no signal one way or the other, so we cannot claim the
    /// cursor is embedded. Returning false here keeps the
    /// previous-frame state machine driven by the next genuine
    /// update.
    #[test]
    fn frame_contains_embedded_cursor_false_when_no_mouse_update() {
        let f = frame_info_for_embedded_test(0, false);
        assert!(!frame_contains_embedded_cursor(&f));
    }

    /// Both predicates inverted: no update *and* pointer marked
    /// visible. Equivalent to the no-update case for our purposes.
    #[test]
    fn frame_contains_embedded_cursor_false_when_no_update_and_visible() {
        let f = frame_info_for_embedded_test(0, true);
        assert!(!frame_contains_embedded_cursor(&f));
    }

    fn luid(lo: u32, hi: i32) -> LUID {
        LUID {
            LowPart: lo,
            HighPart: hi,
        }
    }

    fn make_names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn find_device_name_index_finds_match() {
        let names = make_names(&[r"\\.\DISPLAY1", r"\\.\DISPLAY7"]);
        assert_eq!(
            find_device_name_index(&names, r"\\.\DISPLAY7").expect("match"),
            1
        );
        assert_eq!(
            find_device_name_index(&names, r"\\.\DISPLAY1").expect("match"),
            0
        );
    }

    /// Empty `video_device_name` is a legal-but-unselected state on
    /// fresh installs — the daemon must not silently fall back to
    /// "device index 0" on this path. Hard error so the caller (the
    /// capture-engine factory, ultimately) surfaces a structured
    /// failure to the worker / pipeline. The frontend keeps users from
    /// hitting this in practice by gating submit on a non-empty name.
    #[test]
    fn find_device_name_index_returns_invalid_params_when_empty_string() {
        let names = make_names(&[r"\\.\DISPLAY1"]);
        let err = find_device_name_index(&names, "").expect_err("empty string must error");
        let msg = format!("{}", err);
        assert!(
            msg.contains("video_device_name is empty"),
            "error message must call out the empty selection: {}",
            msg
        );
    }

    #[test]
    fn find_device_name_index_returns_invalid_params_when_no_match() {
        let names = make_names(&[r"\\.\DISPLAY1", r"\\.\DISPLAY7"]);
        let err =
            find_device_name_index(&names, r"\\.\DISPLAY99").expect_err("unknown name must error");
        let msg = format!("{}", err);
        // The Debug formatter double-escapes backslashes, so the
        // assertion targets the human-recognisable suffix that is
        // stable across Display/Debug rendering.
        assert!(
            msg.contains("DISPLAY99") && msg.contains("DISPLAY1") && msg.contains("DISPLAY7"),
            "error message must include the requested name and the enumerated list: {}",
            msg
        );
    }

    /// Failing to find a display through the DXGI enumeration is
    /// indistinguishable from "this is an IDD that DXGI cannot see",
    /// so the error message must always carry the WGC-fallback hint.
    /// The frontend uses this string verbatim from the worker log only
    /// for diagnostics — actual fallback is the user's job once they
    /// see the suggestion.
    #[test]
    fn dxgi_select_by_name_rejects_idd_with_helpful_message() {
        let names = make_names(&[r"\\.\DISPLAY1"]);
        let err = find_device_name_index(&names, r"\\.\DISPLAY99").expect_err("not found");
        let msg = format!("{}", err);
        assert!(
            msg.contains("IDD virtual displays are not exposed through DXGI"),
            "error message must contain the IDD/WGC hint: {}",
            msg
        );
        assert!(
            msg.contains("WGC"),
            "error message must mention WGC as the alternative backend: {}",
            msg
        );
    }

    #[test]
    fn order_adapters_default_in_middle_promoted_to_front() {
        let a = luid(1, 0);
        let b = luid(2, 0);
        let c = luid(3, 0);
        let got = order_adapters_by_default_luid(Some(b), &[a, b, c]);
        assert_eq!(got, vec![1, 0, 2]);
    }

    #[test]
    fn order_adapters_default_not_found_keeps_factory_order() {
        let a = luid(1, 0);
        let b = luid(2, 0);
        let c = luid(3, 0);
        let unknown = luid(99, 0);
        let got = order_adapters_by_default_luid(Some(unknown), &[a, b, c]);
        assert_eq!(got, vec![0, 1, 2]);
    }

    #[test]
    fn order_adapters_default_is_none_keeps_factory_order() {
        let a = luid(1, 0);
        let b = luid(2, 0);
        let c = luid(3, 0);
        let got = order_adapters_by_default_luid(None, &[a, b, c]);
        assert_eq!(got, vec![0, 1, 2]);
    }

    #[test]
    fn order_adapters_single_adapter_is_idempotent() {
        let a = luid(7, 1);
        assert_eq!(order_adapters_by_default_luid(Some(a), &[a]), vec![0]);
        assert_eq!(order_adapters_by_default_luid(None, &[a]), vec![0]);
    }

    #[test]
    fn order_adapters_empty_input_returns_empty() {
        let got = order_adapters_by_default_luid(Some(luid(1, 0)), &[]);
        assert!(got.is_empty());
    }

    // -----------------------------------------------------------------
    // Integration smoke tests for `enumerate_all_outputs` — gated on
    // Windows + #[ignore] so they only run on a real DXGI host via
    // `cargo test -- --ignored`.
    // -----------------------------------------------------------------

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn enumerate_all_outputs_returns_nonempty_on_windows() {
        initialize();
        let outputs = enumerate_all_outputs().expect("enumerate_all_outputs");
        assert!(
            !outputs.is_empty(),
            "at least one DXGI output should exist on a Windows host"
        );
        for o in &outputs {
            log::info!(
                "[enum] adapter_index={} local_output_index={} adapter='{}' display='{}'",
                o.adapter_index,
                o.local_output_index,
                adapter_name_from_desc(&o.adapter_desc),
                from_dxgi_output_desc(&o.desc).device_name,
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn enumerate_all_outputs_finds_lcxl_idd_when_present() {
        initialize();
        let outputs = enumerate_all_outputs().expect("enumerate_all_outputs");
        let lcxl = outputs.iter().find(|o| {
            let info = from_dxgi_output_desc(&o.desc);
            info.display_device_name
                .as_deref()
                .map(|s| s.to_ascii_lowercase().contains("lcxl"))
                .unwrap_or(false)
        });
        assert!(
            lcxl.is_some(),
            "expected to find an output whose EnumDisplayDevicesW DeviceString contains 'Lcxl'. \
             Confirm `cargo run -p poc-indirect-display -- create-device` is running before running this test."
        );
    }

    pub fn initialize() {
        INIT.call_once(|| {
            // initialization code here
            let _ = init_logs(LevelFilter::Debug);
            let result = ScreenRecordManager::set_thread_input_desktop();
            log::info!("set thread desktop result: {:?}", result);
        });
    }

    /// Save screenshot to file
    fn save_screenshot_to_file(
        capture: &mut DxgiImageCapture,
        bmp_path: &Path,
    ) -> Result<(), CaptureError> {
        let capture_result = capture.capture(CaptureRequest {
            cursor_mode: CursorCaptureMode::RenderInFrame,
        })?;
        let frame = capture_result.image;
        log::info!("frame_buffer.len={}", frame.get_data().len());
        let mut rgb_data = vec![0u8; frame.get_data().len()];
        let rgb_data_array = rgb_data.as_mut_slice();

        let src_stride = frame.get_width() * 4;
        let dst_stride = frame.get_width() * 4;
        // convert bgra to rgba
        bgra_to_rgba(
            frame.get_data(),
            src_stride,
            rgb_data_array,
            dst_stride,
            frame.get_width(),
            frame.get_height(),
        )?;
        image::save_buffer(
            bmp_path,
            rgb_data_array,
            frame.get_width(),
            frame.get_height(),
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
        log::info!("saved screenshot to {}", bmp_path.to_string_lossy());
        Ok(())
    }

    /// Hardware + interactive-desktop dependent: needs a live D3D11
    /// adapter and an attached display. Fails on headless CI / RDP-only
    /// sessions with `DXGI_ERROR_DEVICE_HUNG` (0x887A0005) once another
    /// D3D test in the suite has already consumed the device. Run
    /// manually with `cargo test -- --ignored test_screen` on a
    /// machine with a GPU + monitor.
    #[test]
    #[ignore]
    fn test_screen() -> Result<(), CaptureError> {
        initialize();
        let settings = DeskSettings::default();
        let mut capture = DxgiImageCapture::new(&settings)?;

        let list = DxgiImageOutputEnumerator::new().get_output_list()?;
        assert!(!list.is_empty());

        let tmp_dir = PathBuf::from("sample/screenshot");
        std::fs::create_dir_all(tmp_dir.as_path())?;

        for i in 0..10 {
            let name = tmp_dir.join(format!("screenshot_{}.bmp", i));
            save_screenshot_to_file(&mut capture, name.as_path())?;
        }
        //std::fs::remove_dir_all(tmp_dir.as_path())?;

        Ok(())
    }

    unsafe extern "system" fn enum_proc(
        param0: windows_core::PCWSTR,
        param1: LPARAM,
    ) -> windows_core::BOOL {
        let result = unsafe { param0.to_string() };
        let windows_station_list_pointer = param1.0 as *mut Vec<String>;
        if let Ok(name) = result {
            log::info!("add: {}", name);
            let windows_station_list = unsafe { windows_station_list_pointer.as_mut().unwrap() };
            windows_station_list.push(name);
        } else if let Err(e) = result {
            log::error!("failed to add: {}", e);
        }

        windows_core::BOOL::from(true)
    }

    fn list_desktop_by_station_handle(handle: HWINSTA) {
        let mut desktop_list = Vec::<String>::new();
        let desktop_list_pointer = &raw mut desktop_list;
        let lparam = LPARAM(desktop_list_pointer as isize);
        let enum_result = unsafe { EnumDesktopsW(Some(handle), Some(enum_proc), lparam) };
        log::info!("EnumDesktopsW result: {:?}", enum_result);
        log::info!("desktop_list: {:?}", desktop_list);

        let mut settings = DeskSettings::default();

        for desktop_name in desktop_list {
            let mut desktop_name_utf16: Vec<u16> = desktop_name.encode_utf16().collect();
            // add null terminator to the station name utf16
            desktop_name_utf16.push(0);
            let desktop_name_ptr = windows::core::PCWSTR::from_raw(desktop_name_utf16.as_ptr());

            let hdesk_result = unsafe {
                OpenDesktopW(
                    desktop_name_ptr,
                    DESKTOP_CONTROL_FLAGS(0),
                    true,
                    GENERIC_ALL.0,
                )
            };
            let hdesk = match hdesk_result {
                Ok(hdesk) => hdesk,
                Err(e) => {
                    log::error!("Failed to open desktop {}: {}", desktop_name, e);
                    continue;
                }
            };
            let result = unsafe { SetThreadDesktop(hdesk) };

            let _ = unsafe { CloseDesktop(hdesk) };

            if let Err(e) = result {
                log::error!("Failed to set thread desktop {}: {}", desktop_name, e);
                continue;
            }

            let enumerator = DxgiImageOutputEnumerator::new();
            let list_result = enumerator.get_output_list();
            if let Err(e) = list_result {
                log::error!("Failed to get output list {}: {}", desktop_name, e);
                continue;
            }

            let output_list = list_result.unwrap();
            log::info!(
                "Output list for desktop {}: {:?}",
                desktop_name,
                output_list
            );
            drop(enumerator);
            for output in &output_list {
                settings.video_device_name = output.device_name.clone();
                let capture_result = DxgiImageCapture::new(&settings);
                if let Err(e) = capture_result {
                    log::error!("Failed to get screen output {}: {}", desktop_name, e);
                    continue;
                }

                let mut capture = capture_result.unwrap();
                // first frame is black, skip it
                capture
                    .capture(CaptureRequest {
                        cursor_mode: CursorCaptureMode::Disable,
                    })
                    .unwrap();

                let tmp_dir = PathBuf::from("sample");
                let sanitized_name: String = output
                    .device_name
                    .chars()
                    .map(|c| if c == '\\' || c == '.' { '_' } else { c })
                    .collect();
                let name = tmp_dir.join(format!(
                    "screenshot_{}_{}.bmp",
                    desktop_name, sanitized_name
                ));

                save_screenshot_to_file(&mut capture, name.as_path()).unwrap();
            }
        }
    }
    #[test]
    fn test_windows_api() -> Result<(), CaptureError> {
        initialize();
        let is_admin = unsafe { IsUserAnAdmin() };

        log::info!("is user an admin: {}", is_admin.as_bool());
        let mut windows_station_list = Vec::<String>::new();
        let windows_station_list_pointer = &raw mut windows_station_list;
        let lparam = LPARAM(windows_station_list_pointer as isize);
        let result = unsafe { EnumWindowStationsW(Some(enum_proc), lparam) };
        log::info!("EnumWindowStationsW result: {:?}", result);
        log::info!("windows_station_list: {:?}", windows_station_list);

        for station in &windows_station_list {
            log::info!("station: {}", station);
            let mut station_name_utf16: Vec<u16> = station.encode_utf16().collect();
            // add null terminator to the station name utf16
            station_name_utf16.push(0);
            let station_name_ptr = windows::core::PCWSTR::from_raw(station_name_utf16.as_ptr());
            let open_result = unsafe { OpenWindowStationW(station_name_ptr, true, GENERIC_ALL.0) };

            if let Ok(handle) = open_result {
                list_desktop_by_station_handle(handle);

                let close_result = unsafe { CloseWindowStation(handle) };
                log::info!("CloseWindowStation result: {:?}", close_result);
            } else if let Err(e) = open_result {
                log::error!(
                    "OpenWindowStationW error, station: {}, error: {}",
                    station,
                    e
                );
            }
        }
        let result = unsafe { GetProcessWindowStation() };
        if let Ok(handle) = result {
            log::info!("GetProcessWindowStation handle: {:?}", handle);
            list_desktop_by_station_handle(handle);
        } else if let Err(e) = result {
            log::error!("GetProcessWindowStation error: {}", e);
        }

        Ok(())
    }

    // Disabled by default: hangs on `Barrier::wait` waiting for an
    // interactive desktop switch that doesn't happen under
    // `cargo test --workspace`. Re-enable with `cargo test -- --ignored`
    // when investigating desktop-switch behaviour manually.
    #[test]
    #[ignore]
    fn test_switch_desktop() -> Result<(), CaptureError> {
        initialize();
        let current_thread_id = unsafe { GetCurrentThreadId() };
        log::info!("Current thread id: {}", current_thread_id);

        let h_old = unsafe { GetThreadDesktop(current_thread_id) }?;
        let barrier = Arc::new(Barrier::new(2));
        let b = barrier.clone();
        let thread_handle = thread::spawn(move || {
            let h_old = unsafe { GetThreadDesktop(current_thread_id) }.unwrap();
            log::info!(
                "Get thread desktop handle: {:?}, from thread id: {}",
                h_old,
                current_thread_id
            );
            unsafe { SetThreadDesktop(h_old) }.unwrap();
            let settings = DeskSettings::default();
            let mut capture = DxgiImageCapture::new(&settings).unwrap();

            log::info!("Wait for barrier");
            b.wait();
            thread::sleep(std::time::Duration::from_secs(5)); // wait
            log::info!("Start to capture screen");
            let screent_output_result = capture.capture(CaptureRequest {
                cursor_mode: CursorCaptureMode::RenderInFrame,
            });
            if let Err(e) = screent_output_result {
                log::error!("Failed to get screen output: {}", e);

                let mut capture = DxgiImageCapture::new(&settings).unwrap();
                capture
                    .capture(CaptureRequest {
                        cursor_mode: CursorCaptureMode::RenderInFrame,
                    })
                    .unwrap();

                let tmp_dir = PathBuf::from("sample");
                let name = tmp_dir.join("switch_desktop_screenshot_retry.bmp".to_string());

                save_screenshot_to_file(&mut capture, name.as_path()).unwrap();
                return;
            }
            screent_output_result.unwrap();

            let tmp_dir = PathBuf::from("sample");
            let name = tmp_dir.join("switch_desktop_screenshot.bmp".to_string());

            save_screenshot_to_file(&mut capture, name.as_path()).unwrap();
        });

        log::info!("Old desktop handle: {:?}", h_old);
        // add null terminator to the station name utf16
        let desktop_name_ptr = w!("Test");
        barrier.wait();
        let h_new = unsafe {
            CreateDesktopW(
                desktop_name_ptr,
                windows::core::PCWSTR::null(),
                None,
                DESKTOP_CONTROL_FLAGS(0),
                GENERIC_ALL.0,
                None,
            )
        }?;
        log::info!("New desktop handle: {:?}", h_new);
        unsafe { SetThreadDesktop(h_new) }?;
        unsafe { SwitchDesktop(h_new) }?;

        let text_ptr = w!("成功!");
        let caption_ptr = w!("测试!");

        unsafe { MessageBoxW(None, text_ptr, caption_ptr, MB_OK) };
        unsafe { SwitchDesktop(h_old) }?;
        let _ = unsafe { CloseDesktop(h_new) };

        // wait for the thread to finish
        let _ = thread_handle.join();
        Ok(())
    }
}
