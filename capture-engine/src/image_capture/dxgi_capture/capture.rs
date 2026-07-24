use super::*;

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
        // frame (software-cursor path). The payload carries
        // `embedded=true` and `visible=false`; the front-end uses
        // `embedded=true` to keep showing the local CSS cursor
        // sprite (instead of hiding it the way a regular
        // `visible=false` payload would imply) while also
        // surfacing a one-off toast that explains the second
        // cursor in the video frame. The `Embedded` fingerprint
        // is distinct from both Hidden and Shape{...} so toggling
        // between hardware-cursor and software-cursor modes
        // always emits a fresh payload (PartialEq on the enum
        // drives the dedup in the caller).
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
