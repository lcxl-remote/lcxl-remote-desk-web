use std::{backtrace::Backtrace, sync::Arc};

use crate::{
    desk_error::DeskError,
    model::{
        common::ErrorCode,
        record_screen::{DisplayInfo, VERTEX},
        settings::Settings,
    },
};
use log::warn;
use openh264::{
    OpenH264API,
    encoder::{BitRate, IntraFramePeriod},
};
use std::fmt::Debug;
use windows::Win32::{
    Foundation::{GENERIC_ALL, HMODULE},
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_REFERENCE,
            D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_9_1, D3D_FEATURE_LEVEL_10_0,
            D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D11_SRV_DIMENSION_TEXTURE2D,
            Fxc::D3DCompile, ID3DInclude,
        },
        Direct3D11::{
            D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VERTEX_BUFFER, D3D11_BLEND_DESC,
            D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA,
            D3D11_BLEND_ZERO, D3D11_BOX, D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL,
            D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_CREATE_DEVICE_DEBUG, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FLOAT32_MAX,
            D3D11_INPUT_ELEMENT_DESC, D3D11_INPUT_PER_VERTEX_DATA, D3D11_SAMPLER_DESC,
            D3D11_SDK_VERSION, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SUBRESOURCE_DATA,
            D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11BlendState, ID3D11Device,
            ID3D11DeviceContext, ID3D11InputLayout, ID3D11PixelShader, ID3D11RenderTargetView,
            ID3D11SamplerState, ID3D11Texture2D, ID3D11VertexShader,
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
        Hlsl::D3D_COMPILE_STANDARD_FILE_INCLUDE,
    },
    Media::MediaFoundation::{MF_FLOAT2, MF_FLOAT3},
    System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetProcessWindowStation,
        OpenInputDesktop, SetThreadDesktop,
    },
};
use windows_core::{Interface, s};
use yuv::{
    YuvChromaSubsampling, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
    bgra_to_yuv420,
};
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

    pub fn make_rtv(
        &self,
        back_buffer: &ID3D11Texture2D,
    ) -> Result<[Option<ID3D11RenderTargetView>; 1], DeskError> {
        // Create a render target view
        let mut rtv = None;
        unsafe {
            self.device
                .CreateRenderTargetView(back_buffer, None, Some(&mut rtv))
        }?;
        let rtv = [rtv];
        // Set new render target
        unsafe { self.device_context.OMSetRenderTargets(Some(&rtv), None) };

        return Ok(rtv);
    }

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
                s!("main"),
                s!("vs_4_0_level_9_3"),
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
                s!("main"),
                s!("ps_4_0_level_9_3"),
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

    pub fn new(settings: &Settings) -> Result<Arc<Self>, DeskError> {
        // get desktop
        //Self::set_thread_desktop()?;

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
        if settings.desk.enable_d3d_debug {
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
                warn!(
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
        blend_state_desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
        blend_state_desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_ZERO;
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

    pub fn get_output_list(&self) -> Result<Vec<DisplayInfo>, DeskError> {
        let mut output_list = vec![];
        let mut output_index = 0;
        loop {
            let result = unsafe { self.dxgi_adapter.EnumOutputs(output_index) };
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

pub trait ScreenRecordManagerArc {
    fn get_screen_output(&self, output_index: u32) -> Result<ScreenOutput, DeskError>;
}

impl ScreenRecordManagerArc for Arc<ScreenRecordManager> {
    fn get_screen_output(&self, output_index: u32) -> Result<ScreenOutput, DeskError> {
        ScreenOutput::new(self.clone(), output_index)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

pub struct ScreenOutput {
    pub manager: Arc<ScreenRecordManager>,
    pub output_index: u32,
    pub digx_output_desc: DXGI_OUTPUT_DESC,
    pub dup_output: IDXGIOutputDuplication,
    pub dup_output_desc: DXGI_OUTDUPL_DESC,
    pub texture2d: ID3D11Texture2D,
    pub surface: IDXGISurface,
    pub pointer_shape_buffer: Vec<u8>,
    pub last_mouse_update_time: i64,
    pub pointer_position: Point,
    pub pointer_visible: bool,
    pub pointer_shape_info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
}

/// Workaround for DXGI_OUTPUT_DESC.Monitor not being Send + Sync
/// This is only works in single thread, so it is safe to use in this case.
unsafe impl Send for ScreenOutput {}
unsafe impl Sync for ScreenOutput {}

pub const POINTER_SHAPE_TYPE_MONOCHROME: u32 = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
pub const POINTER_SHAPE_TYPE_COLOR: u32 = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32;
pub const POINTER_SHAPE_TYPE_MASKED_COLOR: u32 =
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;

const NUMVERTICES: u32 = 6;
const BPP: i32 = 4;

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

        let mut texture2d = None;
        unsafe {
            screen_record_manager.device.CreateTexture2D(
                &copy_buffer_desc,
                None,
                Some(&mut texture2d),
            )
        }?;
        let texture2d = texture2d.unwrap();

        unsafe { texture2d.SetEvictionPriority(DXGI_RESOURCE_PRIORITY_MAXIMUM.0) };
        let surface = texture2d.cast::<IDXGISurface>()?;

        Ok(ScreenOutput {
            manager: screen_record_manager,
            output_index,
            digx_output_desc,
            dup_output,
            dup_output_desc,
            texture2d,
            surface,
            pointer_shape_buffer: vec![],
            last_mouse_update_time: 0,
            pointer_position: Point::default(),
            pointer_visible: false,
            pointer_shape_info: DXGI_OUTDUPL_POINTER_SHAPE_INFO::default(),
        })
    }
    /// DXGI_ERROR_WAIT_TIMEOUT
    pub fn get_frame(&mut self, draw_mouse: bool) -> Result<SceenFrame, DeskError> {
        let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut desktop_resource: Option<IDXGIResource> = None;

        unsafe {
            let ummap_result = self.surface.Unmap();
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

            self.dup_output
                .AcquireNextFrame(500, &mut frame_info, &mut desktop_resource)?;
        };
        let desktop_resource = desktop_resource.unwrap();

        let acquired_desktop_image = desktop_resource.cast::<ID3D11Texture2D>()?;

        unsafe {
            self.manager
                .device_context
                .CopyResource(&self.texture2d, &acquired_desktop_image)
        };
        // draw mouse cursor if needed
        if draw_mouse {
            self.draw_mouse(&frame_info)?;
        }
        let mut locked_rect = DXGI_MAPPED_RECT::default();

        let frame_buffer = unsafe {
            self.surface.Map(&mut locked_rect, DXGI_MAP_READ)?;
            core::slice::from_raw_parts(
                locked_rect.pBits,
                locked_rect.Pitch as usize * self.dup_output_desc.ModeDesc.Height as usize,
            )
        };

        Ok(SceenFrame {
            frame_info,
            frame_buffer,
        })
    }

    /// Draw mouse cursor on the screen
    pub fn draw_mouse(&mut self, frame_info: &DXGI_OUTDUPL_FRAME_INFO) -> Result<(), DeskError> {
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
        unsafe { self.texture2d.GetDesc(&mut full_desc) };
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

        // Position will be changed based on mouse position
        let mut vertices = [
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

        // Set shader resource properties
        let mut s_desc = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
        s_desc.Format = desc.Format;
        s_desc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
        s_desc.Anonymous.Texture2D.MostDetailedMip = desc.MipLevels - 1;
        s_desc.Anonymous.Texture2D.MipLevels = desc.MipLevels;

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
            let rtv = self.manager.make_rtv(&mut self.texture2d)?;
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
                .OMSetRenderTargets(Some(&rtv), None);
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
        // Copy back to desktop image
        /*
               {
                   let mut mouse_box = D3D11_BOX::default();
                   mouse_box.right = ptr_width as u32;
                   mouse_box.bottom = ptr_height as u32;
                   mouse_box.front = 0;
                   mouse_box.back = 1;
                   unsafe {
                       self.manager.device_context.CopySubresourceRegion(
                           &self.texture2d,
                           0,
                           ptr_left as u32,
                           ptr_top as u32,
                           0,
                           &mouse_tex,
                           0,
                           Some(&mouse_box),
                       )
                   };
               }
        */
        Ok(())
    }

    fn process_mono_and_masked_pointer(
        &mut self,
        init_buffer: &mut Vec<u8>,
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

        unsafe {
            self.manager.device_context.CopySubresourceRegion(
                &copy_buffer,
                0,
                0,
                0,
                0,
                &self.texture2d,
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

pub struct NalInfo {
    pub nal_bytes: bytes::Bytes,
}

pub trait ScreenOutputVideoNal {
    fn get_nal(&mut self) -> Result<NalInfo, DeskError>;
}

#[derive(Debug)]
pub struct YuvPlanarImageWrapper<'a, T>
where
    T: Copy + Debug,
{
    pub inner: YuvPlanarImageMut<'a, T>,
}

impl<'a, T> YuvPlanarImageWrapper<'a, T>
where
    T: Copy + Debug,
{
    pub fn new(inner: YuvPlanarImageMut<'a, T>) -> Self {
        Self { inner }
    }
}

impl openh264::formats::YUVSource for YuvPlanarImageWrapper<'_, u8> {
    fn dimensions(&self) -> (usize, usize) {
        (self.inner.width as usize, self.inner.height as usize)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (
            self.inner.y_stride as usize,
            self.inner.u_stride as usize,
            self.inner.v_stride as usize,
        )
    }

    fn y(&self) -> &[u8] {
        self.inner.y_plane.borrow()
    }

    fn u(&self) -> &[u8] {
        self.inner.u_plane.borrow()
    }

    fn v(&self) -> &[u8] {
        self.inner.v_plane.borrow()
    }
}
pub struct H264ScreenOutput {
    pub manager: Arc<ScreenRecordManager>,
    pub output_index: u32,
    pub screen_output: Option<ScreenOutput>,
    pub encoder: openh264::encoder::Encoder,
}

impl H264ScreenOutput {
    pub fn new(manager: Arc<ScreenRecordManager>, output_index: u32) -> Self {
        let config = openh264::encoder::EncoderConfig::new()
            .intra_frame_period(IntraFramePeriod::from_num_frames(30))
            .bitrate(BitRate::from_bps(10_000_000));
        let api = OpenH264API::from_source();
        let encoder = openh264::encoder::Encoder::with_api_config(api, config).unwrap();
        Self {
            manager,
            output_index,
            screen_output: None,
            encoder,
        }
    }
}

impl ScreenOutputVideoNal for H264ScreenOutput {
    fn get_nal(&mut self) -> Result<NalInfo, DeskError> {
        log::trace!("Start to get screen output frame");
        if self.screen_output.is_none() {
            log::info!("screen output is none, need to create screen output");
            let new_screen_output = self.manager.get_screen_output(self.output_index)?;
            self.screen_output = Some(new_screen_output);
        }
        let mut screen_output = self.screen_output.as_mut().unwrap();
        let width = screen_output.dup_output_desc.ModeDesc.Width;
        let height = screen_output.dup_output_desc.ModeDesc.Height;

        let mut result = screen_output.get_frame(true);
        if let Err(error) = result {
            if let DeskError::WindowsResultError(bt, err) = error {
                if err.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    log::warn!("capture frame timeout, will retry, error={:?}", err);
                    return DeskError::custom_error(
                        ErrorCode::CAPTURE_SCREEN_TIMEOUT_ERROR,
                        format!("capture frame timeout, will retry, error={:?}", err),
                    );
                } else if err.code() == DXGI_ERROR_ACCESS_LOST
                    || err.code() == DXGI_ERROR_INVALID_CALL
                {
                    log::error!(
                        "We lost access to the screen output, need to reinitialize the screen output, error={:?}, backtrace={}",
                        err,
                        bt
                    );
                    self.screen_output = None;
                    let new_screen_output = self.manager.get_screen_output(self.output_index)?;
                    self.screen_output = Some(new_screen_output);
                    screen_output = self.screen_output.as_mut().unwrap();

                    result = screen_output.get_frame(true);
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
        log::trace!(
            "Got screen output frame, info={:?}",
            screen_frame.frame_info
        );

        let src_stride = width * 4;
        let mut planar_image = YuvPlanarImageMut::<u8>::alloc(
            width as u32,
            height as u32,
            YuvChromaSubsampling::Yuv420,
        );

        bgra_to_yuv420(
            &mut planar_image,
            screen_frame.frame_buffer,
            src_stride,
            YuvRange::Limited,
            YuvStandardMatrix::Bt601,
            YuvConversionMode::Balanced,
        )?;
        log::trace!("Converted to YUV420 format");
        let yuv_source = YuvPlanarImageWrapper::<u8>::new(planar_image);

        let encoded_bit_stream = self.encoder.encode(&yuv_source)?;
        log::trace!("Encoded to H.264 format");
        let encoded_bit_bytes = bytes::Bytes::from(encoded_bit_stream.to_vec());
        log::trace!(
            "frame_type={:?}, num_layers={:?}",
            encoded_bit_stream.frame_type(),
            encoded_bit_stream.num_layers()
        );
        Ok(NalInfo {
            nal_bytes: encoded_bit_bytes,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SceenFrame<'a> {
    pub frame_info: DXGI_OUTDUPL_FRAME_INFO,
    pub frame_buffer: &'a [u8],
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
        screent_output: &mut ScreenOutput,
        bmp_path: &Path,
    ) -> Result<(), DeskError> {
        let width = screent_output.dup_output_desc.ModeDesc.Width;
        let height = screent_output.dup_output_desc.ModeDesc.Height;

        let frame = screent_output.get_frame(true)?;
        log::info!(
            "frame_info={:?}, frame_buffer.len={}",
            frame.frame_info,
            frame.frame_buffer.len()
        );
        let mut rgb_data = vec![0u8; frame.frame_buffer.len()];
        let rgb_data_array = rgb_data.as_mut_slice();

        let src_stride = width * 4;
        let dst_stride = width * 4;
        // convert bgra to rgba
        bgra_to_rgba(
            frame.frame_buffer,
            src_stride,
            rgb_data_array,
            dst_stride,
            width,
            height,
        )?;
        image::save_buffer(
            bmp_path,
            rgb_data_array,
            width,
            height,
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
        let settings = Settings::default();
        let manager = ScreenRecordManager::new(&settings)?;
        let list = manager.get_output_list()?;
        assert!(!list.is_empty());

        let mut screent_output = manager.get_screen_output(0)?;
        let tmp_dir = PathBuf::from("sample/screenshot");
        std::fs::create_dir_all(tmp_dir.as_path())?;

        for i in 0..10 {
            let name = tmp_dir.join(format!("screenshot_{}.bmp", i));
            save_screenshot_to_file(&mut screent_output, name.as_path())?;
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

        let settings = Settings::default();
        let manager = ScreenRecordManager::new(&settings).unwrap();

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

            let list_result = manager.get_output_list();
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
            for index in 0..output_list.len() {
                let screent_output_result = manager.get_screen_output(index as u32);
                if let Err(e) = screent_output_result {
                    log::error!("Failed to get screen output {}: {}", desktop_name, e);
                    continue;
                }
                let mut screent_output = screent_output_result.unwrap();
                // first frame is black, skip it
                screent_output.get_frame(false).unwrap();

                let tmp_dir = PathBuf::from("sample");
                let name = tmp_dir.join(format!("screenshot_{}_{}.bmp", desktop_name, index));

                save_screenshot_to_file(&mut screent_output, name.as_path()).unwrap();
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
            let settings = Settings::default();
            let manager = ScreenRecordManager::new(&settings).unwrap();

            log::info!("Wait for barrier");
            b.wait();
            thread::sleep(std::time::Duration::from_secs(5)); // wait
            log::info!("Start to capture screen");
            let screent_output_result = manager.get_screen_output(0);
            if let Err(e) = screent_output_result {
                log::error!("Failed to get screen output: {}", e);
                ScreenRecordManager::set_thread_input_desktop().unwrap();
                let manager = ScreenRecordManager::new(&settings).unwrap();
                let mut screent_output = manager.get_screen_output(0).unwrap();
                screent_output.get_frame(false).unwrap();

                let tmp_dir = PathBuf::from("sample");
                let name = tmp_dir.join(format!("switch_desktop_screenshot_retry.bmp"));

                save_screenshot_to_file(&mut screent_output, name.as_path()).unwrap();
                return;
            }
            let mut screent_output = screent_output_result.unwrap();
            // first frame is black, skip it
            screent_output.get_frame(false).unwrap();

            let tmp_dir = PathBuf::from("sample");
            let name = tmp_dir.join(format!("switch_desktop_screenshot.bmp"));

            save_screenshot_to_file(&mut screent_output, name.as_path()).unwrap();
        });

        log::info!("Old desktop handle: {:?}", h_old);
        // add null terminator to the station name utf16
        let desktop_name_utf16: Vec<u16> = "Test".encode_utf16().chain([0u16]).collect();
        let desktop_name_ptr = windows::core::PCWSTR::from_raw(desktop_name_utf16.as_ptr());
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

        let text_utf16: Vec<u16> = "成功!".encode_utf16().chain([0u16]).collect();
        let text_ptr = windows::core::PCWSTR::from_raw(text_utf16.as_ptr());

        let caption_utf16: Vec<u16> = "测试!".encode_utf16().chain([0u16]).collect();
        let caption_ptr = windows::core::PCWSTR::from_raw(caption_utf16.as_ptr());

        unsafe { MessageBoxW(None, text_ptr, caption_ptr, MB_OK) };
        unsafe { SwitchDesktop(h_old) }?;
        let _ = unsafe { CloseDesktop(h_new) };

        // wait for the thread to finish
        let _ = thread_handle.join();
        Ok(())
    }
}
