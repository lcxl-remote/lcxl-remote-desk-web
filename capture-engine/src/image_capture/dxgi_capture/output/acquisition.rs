use super::*;

impl ScreenOutput {
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
}
