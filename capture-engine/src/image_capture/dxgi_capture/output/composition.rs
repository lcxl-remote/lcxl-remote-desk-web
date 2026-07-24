use super::*;

impl ScreenOutput {
    /// Reads the move-rect and dirty-rect metadata reported by
    /// `IDXGIOutputDuplication` for the current frame. Returns a pair
    /// of independently-owned byte buffers so the caller can decode
    /// them at leisure without worrying about the underlying scratch
    /// buffer being overwritten on the second Get* call.
    ///
    /// The DXGI API reuses a single caller-supplied buffer for both
    /// queries (move first, dirty second); we copy the move bytes out
    /// before the dirty query runs.
    pub(super) fn read_frame_metadata(
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
    pub(super) fn copy_move_rects(
        &mut self,
        moves: &[DXGI_OUTDUPL_MOVE_RECT],
    ) -> Result<(), CaptureError> {
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
    pub(super) fn compose_dirty_rects(
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
}
