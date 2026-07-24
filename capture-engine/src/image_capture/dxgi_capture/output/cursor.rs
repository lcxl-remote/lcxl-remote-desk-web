use super::*;

impl ScreenOutput {
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
