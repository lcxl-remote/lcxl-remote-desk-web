use std::{backtrace::Backtrace, sync::Arc};

use windows::Win32::{
    Foundation::{GENERIC_ALL, HMODULE, RECT},
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
            D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FLOAT32_MAX, D3D11_INPUT_ELEMENT_DESC,
            D3D11_INPUT_PER_VERTEX_DATA, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION,
            D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIEWPORT,
            D3D11CreateDevice, ID3D11BlendState, ID3D11Device, ID3D11DeviceContext,
            ID3D11InputLayout, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState,
            ID3D11Texture2D, ID3D11VertexShader,
        },
        Dxgi::{
            Common::{
                DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_R32G32B32_FLOAT,
            },
            DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_INVALID_CALL,
            DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_MAP_READ, DXGI_MAPPED_RECT,
            DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, DXGI_OUTPUT_DESC,
            DXGI_RESOURCE_PRIORITY_MAXIMUM, IDXGIAdapter, IDXGIDevice, IDXGIOutput1,
            IDXGIOutputDuplication, IDXGIResource, IDXGISurface,
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
    desk_error::DeskError,
    model::{
        capture::{DisplayInfo, DisplayRect, ImageCapture, ImageInfo, ImageType},
        common::ErrorCode,
        settings::DeskSettings,
    },
};

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
    pub fn set_thread_input_desktop() -> Result<(), DeskError> {
        unsafe {
            let result = GetProcessWindowStation();
            if let Err(err) = result {
                log::error!("GetProcessWindowStation failed, error: {:?}", err);
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
                log::warn!("Failed to close desktop, ignore, error: {:?}", err);
            }
        };
        Ok(())
    }

    /// make_rtv creates a render target view for the given back buffer texture.
    pub fn make_rtv(
        &self,
        back_buffer: &ID3D11Texture2D,
    ) -> Result<[Option<ID3D11RenderTargetView>; 1], DeskError> {
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
        return Ok(rtv);
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
    ) -> Result<(ID3D11VertexShader, ID3D11InputLayout, ID3D11PixelShader), DeskError> {
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
        if let Err(complie_error) = compile_result {
            if let Some(blob) = error_msg {
                // ansi format string?
                let blob_array = unsafe {
                    core::slice::from_raw_parts(
                        blob.GetBufferPointer() as *const u8,
                        blob.GetBufferSize(),
                    )
                };
                let error_message = String::from_utf8_lossy(blob_array);
                log::error!("Vertex Shader Compile Error: {}", error_message);
                return Err(DeskError::from(complie_error));
            }
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
        if let Err(complie_error) = compile_result {
            if let Some(blob) = error_msg {
                // ansi format string?
                let blob_array = unsafe {
                    core::slice::from_raw_parts(
                        blob.GetBufferPointer() as *const u8,
                        blob.GetBufferSize(),
                    )
                };
                let error_message = String::from_utf8_lossy(blob_array);
                log::error!("Pixel Shader Compile Error: {}", error_message);
                return Err(DeskError::from(complie_error));
            }
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

    pub fn new(settings: &DeskSettings) -> Result<Arc<Self>, DeskError> {
        // get desktop
        Self::set_thread_input_desktop()?;

        // init dxgi factory
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
        let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        if settings.enable_d3d_debug {
            log::info!("Enable d3d debug flag");
            flags |= D3D11_CREATE_DEVICE_DEBUG;
        }

        let mut device = None;
        //let mut feature_level = D3D_FEATURE_LEVEL_11_1;

        let mut device_context = None;
        let mut result = Ok(());

        for driver_type in driver_types {
            result = unsafe {
                D3D11CreateDevice(
                    None,
                    driver_type,
                    HMODULE::default(),
                    flags,
                    Some(&feature_levels),
                    //None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    //Some(&mut feature_level),
                    None,
                    Some(&mut device_context),
                )
            };
            if let Err(error) = result.clone() {
                log::warn!(
                    "Failed to create device with driver type {:?}, code: {}",
                    driver_type,
                    error.code()
                );
            } else if let Ok(_) = result.clone() {
                break;
            }
        }
        // Check if device creation was successful
        result?;

        let device = device.unwrap();
        let device_context = device_context.unwrap();

        let dxgi_device = device.cast::<IDXGIDevice>()?;
        let dxgi_adapter = unsafe { dxgi_device.GetParent::<IDXGIAdapter>() }?;

        // Create the sample state
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
        // Create the blend state
        let mut blend_state_desc = D3D11_BLEND_DESC::default();

        blend_state_desc.AlphaToCoverageEnable = false.into();
        blend_state_desc.IndependentBlendEnable = false.into();
        blend_state_desc.RenderTarget[0].BlendEnable = true.into();
        blend_state_desc.RenderTarget[0].SrcBlend = D3D11_BLEND_SRC_ALPHA;
        blend_state_desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
        blend_state_desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
        // big thanks to https://github.com/MirrorX-Desktop/MirrorX/blob/master/mirrorx_core/src/component/desktop/windows/duplicator.rs#L1013C51-L1013C80
        blend_state_desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_INV_DEST_ALPHA; //D3D11_BLEND_ONE 
        blend_state_desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_ONE; //D3D11_BLEND_ZERO
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
    fn get_screen_output(&self, output_index: u32) -> Result<ScreenOutput, DeskError>;
}

impl ScreenRecordManagerArc for Arc<ScreenRecordManager> {
    fn get_screen_output(&self, output_index: u32) -> Result<ScreenOutput, DeskError> {
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
    pub render_target_texture_2d: ID3D11Texture2D,
    pub rtv: [Option<ID3D11RenderTargetView>; 1],
}

impl ScreenOutput {
    pub fn new(
        screen_record_manager: Arc<ScreenRecordManager>,
        output_index: u32,
    ) -> Result<Self, DeskError> {
        let output = unsafe { screen_record_manager.dxgi_adapter.EnumOutputs(output_index) }?;

        let digx_output_desc = unsafe { output.GetDesc() }?;
        let output1 = output.cast::<IDXGIOutput1>()?;
        // get the device from the manager and pass it to DuplicateOutput
        let pdevice = &screen_record_manager.device;

        let dup_output = unsafe { output1.DuplicateOutput(pdevice) }?;
        let dup_output_desc = unsafe { dup_output.GetDesc() };
        log::info!(
            "output_index {}, dxgi_output_desc {:?}, dup_output_desc {:?}",
            output_index,
            digx_output_desc,
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

        // Create render target texture
        let render_target_texture_2d = ScreenOutput::create_render_target_texture(
            &screen_record_manager.device,
            dup_output_desc.ModeDesc.Width,
            dup_output_desc.ModeDesc.Height,
        )?;

        let rtv = screen_record_manager.make_rtv(&render_target_texture_2d)?;

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
        })
    }

    /// DXGI_ERROR_WAIT_TIMEOUT
    pub fn get_frame<'a>(&mut self, draw_mouse: bool) -> Result<SceenFrame<'a>, DeskError> {
        let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut desktop_resource: Option<IDXGIResource> = None;

        unsafe {
            self.dup_output
                .AcquireNextFrame(500, &mut frame_info, &mut desktop_resource)?;
        }
        let desktop_resource = desktop_resource.unwrap();

        let acquired_desktop_image = desktop_resource.cast::<ID3D11Texture2D>()?;

        self.draw_desktop(&acquired_desktop_image)?;

        // draw mouse cursor if needed
        if draw_mouse {
            self.draw_mouse(&frame_info, &acquired_desktop_image)?;
        }
        // Copy render target texture to shared texture
        unsafe {
            self.manager
                .device_context
                .CopyResource(&self.copy_buffer_texture_2d, &self.render_target_texture_2d);
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

        Ok(SceenFrame {
            height: self.dup_output_desc.ModeDesc.Height,
            width: self.dup_output_desc.ModeDesc.Width,
            frame_buffer,
            copy_buffer_surface: self.copy_buffer_surface.clone(),
            dup_output: self.dup_output.clone(),
        })
    }

    /// Create render target texture
    pub fn create_render_target_texture(
        device: &ID3D11Device,
        desktop_width: u32,
        desktop_height: u32,
    ) -> Result<ID3D11Texture2D, DeskError> {
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

    /// Draw desktop to the render target texture
    pub fn draw_desktop(
        &mut self,
        acquired_desktop_image: &ID3D11Texture2D,
    ) -> Result<(), DeskError> {
        // Desktop dimensions
        let mut frame_desc: D3D11_TEXTURE2D_DESC = D3D11_TEXTURE2D_DESC::default();
        unsafe { acquired_desktop_image.GetDesc(&mut frame_desc) };

        let mut shader_desc = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
        shader_desc.Format = frame_desc.Format;
        shader_desc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
        shader_desc.Anonymous.Texture2D.MostDetailedMip = frame_desc.MipLevels - 1;
        shader_desc.Anonymous.Texture2D.MipLevels = frame_desc.MipLevels;

        // Create new shader resource view
        let mut shader_resource = None;
        unsafe {
            self.manager.device.CreateShaderResourceView(
                acquired_desktop_image,
                Some(&shader_desc),
                Some(&mut shader_resource),
            )
        }?;

        // Set resources

        let stride = size_of::<VERTEX>() as u32;
        let offset = 0;
        let blend_factor = [0.0f32, 0.0f32, 0.0f32, 0.0f32];

        unsafe {
            self.manager
                .device_context
                .OMSetBlendState(None, Some(&blend_factor), 0xffffffff);
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
                .PSSetShaderResources(0, Some(&[shader_resource]));
            self.manager
                .device_context
                .PSSetSamplers(0, Some(&self.manager.sampler_linear));
            self.manager
                .device_context
                .IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        }
        let mut buffer_desc = D3D11_BUFFER_DESC::default();

        buffer_desc.Usage = D3D11_USAGE_DEFAULT;
        buffer_desc.ByteWidth = size_of::<VERTEX>() as u32 * NUMVERTICES;
        buffer_desc.BindFlags = D3D11_BIND_VERTEX_BUFFER.0 as u32;
        buffer_desc.CPUAccessFlags = 0;

        let mut init_data = D3D11_SUBRESOURCE_DATA::default();
        init_data.pSysMem = VERTICES.as_ptr() as *const _;
        // Create vertex buffer
        let mut vertex_buffer = None;
        unsafe {
            self.manager.device.CreateBuffer(
                &buffer_desc,
                Some(&init_data),
                Some(&mut vertex_buffer),
            )?;

            self.manager.device_context.IASetVertexBuffers(
                0,
                1,
                Some(&vertex_buffer),
                Some(&stride),
                Some(&offset),
            );

            // Draw textured quad onto render target
            self.manager.device_context.Draw(NUMVERTICES, 0);
        }

        Ok(())
    }

    /// Draw mouse cursor on the screen
    pub fn draw_mouse(
        &mut self,
        frame_info: &DXGI_OUTDUPL_FRAME_INFO,
        acquired_desktop_image: &ID3D11Texture2D,
    ) -> Result<(), DeskError> {
        // A non-zero mouse update timestamp indicates that there is a mouse position update and optionally a shape change

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
                        return Err(DeskError::from(error));
                    }
                    log::debug!("Pointer shape info: {:?}", self.pointer_shape_info);
                }
            } else {
                // mouse is not visible, clear the pointer shape buffer
                self.pointer_shape_buffer = vec![];
            }
        }

        if !self.pointer_visible {
            log::trace!("Pointer is not visible, skipping drawing pointer shape.");
            // If the pointer is not visible, we don't need to draw anything. Just return.
            return Ok(());
        }

        let is_mono = self.pointer_shape_info.Type == POINTER_SHAPE_TYPE_MONOCHROME;
        // Desktop dimensions
        let mut full_desc: D3D11_TEXTURE2D_DESC = D3D11_TEXTURE2D_DESC::default();
        unsafe { acquired_desktop_image.GetDesc(&mut full_desc) };
        let desktop_width = full_desc.Width as i32;
        let desktop_height = full_desc.Height as i32;

        // Center of desktop dimensions
        let center_x = desktop_width / 2;
        let center_y = desktop_height / 2;
        // Pointer position
        let given_left = self.pointer_position.x;
        let given_top = self.pointer_position.y;

        // Figure out if any adjustment is needed for out of bound positions
        let ptr_width = if given_left < 0 {
            given_left + self.pointer_shape_info.Width as i32
        } else if (given_left + self.pointer_shape_info.Width as i32) > desktop_width as i32 {
            desktop_width as i32 - given_left
        } else {
            self.pointer_shape_info.Width as i32
        };

        if is_mono {
            self.pointer_shape_info.Height = self.pointer_shape_info.Height / 2;
        }

        let ptr_height = if given_top < 0 {
            given_top + self.pointer_shape_info.Height as i32
        } else if (given_top + self.pointer_shape_info.Height as i32) > desktop_height as i32 {
            desktop_height as i32 - given_top
        } else {
            self.pointer_shape_info.Height as i32
        };

        if is_mono {
            self.pointer_shape_info.Height = self.pointer_shape_info.Height * 2;
        }

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
                    acquired_desktop_image,
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
        vertices[0].pos.y = -1.0 * ((ptr_top + ptr_height) - center_y) as f32 / center_y as f32;
        vertices[1].pos.x = (ptr_left - center_x) as f32 / center_x as f32;
        vertices[1].pos.y = -1.0 * (ptr_top - center_y) as f32 / center_y as f32;
        vertices[2].pos.x = ((ptr_left + ptr_width) - center_x) as f32 / center_x as f32;
        vertices[2].pos.y = -1.0 * ((ptr_top + ptr_height) - center_y) as f32 / center_y as f32;
        vertices[3].pos.x = vertices[2].pos.x;
        vertices[3].pos.y = vertices[2].pos.y;
        vertices[4].pos.x = vertices[1].pos.x;
        vertices[4].pos.y = vertices[1].pos.y;
        vertices[5].pos.x = ((ptr_left + ptr_width) - center_x) as f32 / center_x as f32;
        vertices[5].pos.y = -1.0 * (ptr_top - center_y) as f32 / center_y as f32;

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
                .OMSetRenderTargets(Some(&self.rtv), None);
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
        acquired_desktop_image: &ID3D11Texture2D,
        is_mono: bool,
        ptr_width: i32,
        ptr_height: i32,
        ptr_left: i32,
        ptr_top: i32,
        given_left: i32,
        given_top: i32,
    ) -> Result<(), DeskError> {
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
                acquired_desktop_image,
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
            (-1 * given_left) as u32
        } else {
            0
        };
        let skip_y = if given_top < 0 {
            (-1 * given_top) as u32
        } else {
            0
        };

        if is_mono {
            for row in 0..ptr_height {
                // Set mask
                let mut mask = 0x80u8;
                mask = mask >> (skip_x % 8);
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
                        0xFFFFFFFF as u32
                    } else {
                        0xFF000000
                    };
                    let xor_mask_32 = if xor_mask != 0 {
                        0x00FFFFFF as u32
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
                        mask = mask >> 1;
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

#[derive(Debug, Clone)]
pub struct SceenFrame<'a> {
    pub height: u32,
    pub width: u32,
    pub frame_buffer: &'a [u8],
    pub copy_buffer_surface: IDXGISurface,
    pub dup_output: IDXGIOutputDuplication,
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
}

pub struct DigxImageCapture {
    pub manager: Arc<ScreenRecordManager>,
    pub output_index: u32,
    pub screen_output: Option<ScreenOutput>,
}

impl DigxImageCapture {
    pub fn new(settings: &DeskSettings) -> Result<Self, DeskError> {
        let manager = ScreenRecordManager::new(settings)?;
        let screen_output = Some(ScreenOutput::new(
            manager.clone(),
            settings.video_device_index,
        )?);
        Ok(DigxImageCapture {
            manager,
            screen_output,
            output_index: settings.video_device_index,
        })
    }
}

impl ImageCapture for DigxImageCapture {
    fn capture(&mut self, show_mouse: bool) -> Result<Box<dyn ImageInfo + Send + Sync>, DeskError> {
        log::trace!("Start to get screen output frame");
        if self.screen_output.is_none() {
            self.screen_output = Some(ScreenOutput::new(self.manager.clone(), self.output_index)?);
        }
        let screen_output = self.screen_output.as_mut().unwrap();
        let result = screen_output.get_frame(show_mouse);
        if let Err(error) = result {
            if let DeskError::WindowsResultError(bt, err) = error {
                if err.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    log::warn!("capture frame timeout, will retry, error={:?}", err);

                    return DeskError::custom_error(
                        ErrorCode::CAPTURE_SCREEN_NEED_RETRY,
                        format!("capture frame timeout, will retry, error={:?}", err),
                    );
                } else if err.code() == DXGI_ERROR_ACCESS_LOST
                    || err.code() == DXGI_ERROR_INVALID_CALL
                {
                    self.screen_output = None;

                    return DeskError::custom_error(
                        ErrorCode::CAPTURE_SCREEN_NEED_RETRY,
                        format!("capture frame is lost, will retry, error={:?}", err),
                    );
                } else {
                    if err.code() == DXGI_ERROR_DEVICE_REMOVED {
                        let removed_reason =
                            unsafe { self.manager.device.GetDeviceRemovedReason() };
                        log::error!("Device removed reason: {:?}", removed_reason);
                        return Err(DeskError::WindowsResultError(Backtrace::disabled(), err));
                    }
                    return Err(DeskError::WindowsResultError(bt, err));
                }
            } else {
                return Err(error);
            }
        }

        let screen_frame = result?;
        Ok(Box::new(screen_frame))
    }

    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, DeskError> {
        let mut output_list = vec![];
        let mut output_index = 0;
        loop {
            let result = unsafe { self.manager.dxgi_adapter.EnumOutputs(output_index) };
            if let Ok(output) = result {
                let output_desc: DXGI_OUTPUT_DESC = unsafe { output.GetDesc() }?;

                output_list.push(DisplayInfo::from(output_desc));
            } else if let Err(error) = result {
                if error.code() != DXGI_ERROR_NOT_FOUND {
                    log::error!(
                        "Failed to enumerate outputs, code: {}, message: {}",
                        error.code(),
                        error.message()
                    );
                    return Err(DeskError::from(error));
                }
                log::warn!(
                    "Output index not found, finished enumeration. Total outputs found: {}",
                    output_index
                );
                break;
            }
            output_index += 1;
        }
        Ok(output_list)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::thread;

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

    pub fn initialize() {
        INIT.call_once(|| {
            // initialization code here
            let result = env_logger::builder()
                .format_timestamp_micros()
                .filter_level(LevelFilter::Trace)
                .try_init();
            if let Err(e) = result {
                log::warn!("Failed to initialize logger: {:?}", e);
            }
            let result = ScreenRecordManager::set_thread_input_desktop();
            log::info!("set thread desktop result: {:?}", result);
        });
    }

    /// Save screenshot to file
    fn save_screenshot_to_file(
        capture: &mut DigxImageCapture,
        bmp_path: &Path,
    ) -> Result<(), DeskError> {
        let frame = capture.capture(true)?;
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
        log::info!(
            "saved screenshot to {}",
            bmp_path.to_string_lossy().to_string()
        );
        Ok(())
    }

    #[test]
    fn test_screen() -> Result<(), DeskError> {
        initialize();
        let settings = DeskSettings::default();
        let mut capture = DigxImageCapture::new(&settings)?;

        let list = capture.get_output_list()?;
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
            log::error!("failed to add: {:?}", e);
        }

        return windows_core::BOOL::from(true);
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
            if let Err(e) = hdesk_result {
                log::error!("Failed to open desktop {}: {}", desktop_name, e);
                continue;
            }

            let hdesk = hdesk_result.unwrap();
            let result = unsafe { SetThreadDesktop(hdesk) };

            let _ = unsafe { CloseDesktop(hdesk) };

            if let Err(e) = result {
                log::error!("Failed to set thread desktop {}: {}", desktop_name, e);
                continue;
            }

            let capture = DigxImageCapture::new(&settings).unwrap();
            let list_result = capture.get_output_list();
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
            drop(capture);
            for index in 0..output_list.len() {
                settings.video_device_index = index as u32;
                let capture_result = DigxImageCapture::new(&settings);
                if let Err(e) = capture_result {
                    log::error!("Failed to get screen output {}: {}", desktop_name, e);
                    continue;
                }

                let mut capture = capture_result.unwrap();
                // first frame is black, skip it
                capture.capture(false).unwrap();

                let tmp_dir = PathBuf::from("sample");
                let name = tmp_dir.join(format!("screenshot_{}_{}.bmp", desktop_name, index));

                save_screenshot_to_file(&mut capture, name.as_path()).unwrap();
            }
        }
    }
    #[test]
    fn test_windows_api() -> Result<(), DeskError> {
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

    #[test]
    fn test_switch_desktop() -> Result<(), DeskError> {
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
            let mut capture = DigxImageCapture::new(&settings).unwrap();

            log::info!("Wait for barrier");
            b.wait();
            thread::sleep(std::time::Duration::from_secs(5)); // wait
            log::info!("Start to capture screen");
            let screent_output_result = capture.capture(true);
            if let Err(e) = screent_output_result {
                log::error!("Failed to get screen output: {}", e);

                let mut capture = DigxImageCapture::new(&settings).unwrap();
                capture.capture(true).unwrap();

                let tmp_dir = PathBuf::from("sample");
                let name = tmp_dir.join(format!("switch_desktop_screenshot_retry.bmp"));

                save_screenshot_to_file(&mut capture, name.as_path()).unwrap();
                return;
            }
            screent_output_result.unwrap();

            let tmp_dir = PathBuf::from("sample");
            let name = tmp_dir.join(format!("switch_desktop_screenshot.bmp"));

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
