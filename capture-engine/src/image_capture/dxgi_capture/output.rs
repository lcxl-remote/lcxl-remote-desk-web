use super::*;

mod acquisition;
mod composition;
mod cursor;

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
        // garbage pixels outside the valid region.
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
