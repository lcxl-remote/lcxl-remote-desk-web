use super::*;

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
        let vertex_shader_code = include_str!("../shaders/VertexShader.hlsl");
        let pixel_shader_code = include_str!("../shaders/PixelShader.hlsl");

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
