//! Pure helpers backing the DXGI desktop duplication composition
//! pipeline. Lives in its own module so it can be unit-tested without
//! a D3D11 device — every function here is `Send + Sync`, takes only
//! POD inputs, and emits POD outputs. The Win32 IO side
//! (`GetFrameMoveRects` / `GetFrameDirtyRects` / `CopySubresourceRegion`
//! / draw call submission) stays in `dxgi_capture.rs`.
//!
//! Background: DXGI's `AcquireNextFrame` returns a texture whose pixel
//! data is "valid only in the dirty and move regions of the bitmap"
//! (MSDN). Applications must compose the current desktop from the
//! previous desktop plus the dirty + move regions. The full-quad
//! blit the old implementation used in `draw_desktop` violates this
//! invariant and causes flashes of garbage data ("black bars") on
//! moving content. The functions here support a per-rect composition
//! pipeline modelled on the official MSDN sample (DisplayManager.cpp).

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dxgi::{DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTDUPL_POINTER_SHAPE_INFO};
use windows::Win32::Media::MediaFoundation::{MF_FLOAT2, MF_FLOAT3};

use crate::model::image_capture::DirtyRect;

use super::dxgi_capture::{POINTER_SHAPE_TYPE_MONOCHROME, VERTEX};

/// Upper bound on the number of rects we report as a dirty_hint to
/// downstream partial-YUV. Beyond this we return `None` and let the
/// downstream stage do a full conversion. The compositing pipeline
/// itself is *not* affected by this limit — every move + dirty rect
/// is always applied to the persistent render target.
pub const MAX_DIRTY_HINT_RECTS: usize = 64;

/// Snapshot of cursor state in a single frame. `rect` is only valid
/// when `visible == true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
    pub visible: bool,
    pub rect: DirtyRect,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            visible: false,
            rect: DirtyRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
        }
    }
}

/// DXGI monochrome cursor shape buffers concatenate the AND mask and
/// XOR mask vertically, so the reported Height is the *sum* of both
/// masks' heights. The actual display height is `Height / 2`. Color
/// and masked-color cursors carry a single bitmap whose Height is the
/// display height as-is.
pub fn cursor_display_size(shape_info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO) -> (u32, u32) {
    let h = if shape_info.Type == POINTER_SHAPE_TYPE_MONOCHROME {
        shape_info.Height / 2
    } else {
        shape_info.Height
    };
    (shape_info.Width, h)
}

/// Parses the raw byte slice returned by `IDXGIOutputDuplication::
/// GetFrameMoveRects` into a `Vec<DXGI_OUTDUPL_MOVE_RECT>`.
///
/// If `bytes.len()` is not a multiple of `size_of::<MOVE_RECT>()` the
/// trailing remainder is ignored. The caller is expected to log when
/// this happens — it indicates a driver / API contract violation.
pub fn parse_move_rects(bytes: &[u8]) -> Vec<DXGI_OUTDUPL_MOVE_RECT> {
    let stride = std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
    let count = bytes.len() / stride;
    let mut out: Vec<DXGI_OUTDUPL_MOVE_RECT> = Vec::with_capacity(count);
    for i in 0..count {
        let mut entry = DXGI_OUTDUPL_MOVE_RECT::default();
        // SAFETY: `bytes[i*stride .. i*stride+stride]` is in-bounds by
        // construction; both source and destination are POD with C
        // layout and no padding-sensitive invariants, so a byte-wise
        // memcpy reconstitutes a valid value.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(i * stride),
                &mut entry as *mut _ as *mut u8,
                stride,
            );
        }
        out.push(entry);
    }
    out
}

/// Parses the raw byte slice returned by `IDXGIOutputDuplication::
/// GetFrameDirtyRects` into a `Vec<RECT>`. Same trailing-remainder
/// policy as `parse_move_rects`.
pub fn parse_dirty_rects(bytes: &[u8]) -> Vec<RECT> {
    let stride = std::mem::size_of::<RECT>();
    let count = bytes.len() / stride;
    let mut out: Vec<RECT> = Vec::with_capacity(count);
    for i in 0..count {
        let mut entry = RECT::default();
        // SAFETY: see `parse_move_rects`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(i * stride),
                &mut entry as *mut _ as *mut u8,
                stride,
            );
        }
        out.push(entry);
    }
    out
}

/// Computes the `(source_rect, destination_rect)` pair to feed into
/// `CopySubresourceRegion` when relocating a move region. Mirrors
/// `DisplayManager.cpp::SetMoveRect` from the MSDN sample, identity
/// rotation only — this project's render target shares the
/// `ModeDesc.Width × Height` coordinate space with the acquired
/// desktop image, so no rotation transform is needed.
///
/// Width/height are derived from `mv.DestinationRect`; the source
/// rect starts at `mv.SourcePoint` and has the same dimensions.
pub fn set_move_rect(mv: &DXGI_OUTDUPL_MOVE_RECT) -> (RECT, RECT) {
    let dest = mv.DestinationRect;
    let w = dest.right - dest.left;
    let h = dest.bottom - dest.top;
    let src = RECT {
        left: mv.SourcePoint.x,
        top: mv.SourcePoint.y,
        right: mv.SourcePoint.x + w,
        bottom: mv.SourcePoint.y + h,
    };
    (src, dest)
}

/// Generates the six vertices (two CCW triangles) needed to render a
/// single dirty rect onto the persistent render target. Positions are
/// in normalised device coordinates (Y up, -1..1); texture coordinates
/// are normalised to the source texture (Y down, 0..1).
///
/// Identity rotation only. In identity, `dirty` doubles as both the
/// destination rect on the render target *and* the source rect on
/// the acquired desktop texture, so the function takes one rect.
/// `full_w` / `full_h` are the render target dimensions; `this_w` /
/// `this_h` are the acquired desktop texture dimensions. They are
/// equal in the single-output case but the formulas keep them
/// separate to match `DisplayManager.cpp::SetDirtyVert`.
pub fn dirty_rect_to_vertices(
    dirty: RECT,
    full_w: i32,
    full_h: i32,
    this_w: i32,
    this_h: i32,
) -> [VERTEX; 6] {
    let center_x = (full_w / 2).max(1) as f32;
    let center_y = (full_h / 2).max(1) as f32;

    let x0 = (dirty.left as f32 - center_x) / center_x;
    let x1 = (dirty.right as f32 - center_x) / center_x;
    // D3D pixel-space Y grows downward; NDC Y grows upward — flip.
    let y_bot = -(dirty.bottom as f32 - center_y) / center_y;
    let y_top = -(dirty.top as f32 - center_y) / center_y;

    let inv_tw = if this_w > 0 { 1.0 / this_w as f32 } else { 0.0 };
    let inv_th = if this_h > 0 { 1.0 / this_h as f32 } else { 0.0 };
    let u0 = dirty.left as f32 * inv_tw;
    let u1 = dirty.right as f32 * inv_tw;
    let v_top = dirty.top as f32 * inv_th;
    let v_bot = dirty.bottom as f32 * inv_th;

    let bottom_left = VERTEX {
        pos: MF_FLOAT3 {
            x: x0,
            y: y_bot,
            z: 0.0,
        },
        tex_coord: MF_FLOAT2 { x: u0, y: v_bot },
    };
    let top_left = VERTEX {
        pos: MF_FLOAT3 {
            x: x0,
            y: y_top,
            z: 0.0,
        },
        tex_coord: MF_FLOAT2 { x: u0, y: v_top },
    };
    let bottom_right = VERTEX {
        pos: MF_FLOAT3 {
            x: x1,
            y: y_bot,
            z: 0.0,
        },
        tex_coord: MF_FLOAT2 { x: u1, y: v_bot },
    };
    let top_right = VERTEX {
        pos: MF_FLOAT3 {
            x: x1,
            y: y_top,
            z: 0.0,
        },
        tex_coord: MF_FLOAT2 { x: u1, y: v_top },
    };
    [
        bottom_left,
        top_left,
        bottom_right,
        bottom_right,
        top_left,
        top_right,
    ]
}

/// Aligns rect bounds outward to even pixel boundaries (YUV420 chroma
/// subsampling) and clamps to the frame dimensions. Used when feeding
/// rects to downstream YUV partial-update. Crate-internal mirror of
/// `dxgi_capture::align_and_clamp` so the unit tests can exercise it
/// without pulling in the Win32-heavy parent module.
fn align_and_clamp(
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    img_w: u32,
    img_h: u32,
) -> DirtyRect {
    let ax = (left & !1).max(0) as u32;
    let ay = (top & !1).max(0) as u32;
    let ar = ((right + 1) & !1).min(img_w as i32).max(0) as u32;
    let ab = ((bottom + 1) & !1).min(img_h as i32).max(0) as u32;
    DirtyRect {
        x: ax,
        y: ay,
        width: ar.saturating_sub(ax),
        height: ab.saturating_sub(ay),
    }
}

/// Derives the cursor's drawn rect in render-target pixel coordinates
/// from its position and shape descriptor. Used by the caller to
/// build [`CursorState`] inputs to [`build_dirty_hint`] and to record
/// `last_cursor_rect` after `draw_mouse_into`.
pub fn cursor_rect_from_state(
    pointer_x: i32,
    pointer_y: i32,
    shape_info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    frame_w: u32,
    frame_h: u32,
) -> DirtyRect {
    let (cw, ch) = cursor_display_size(shape_info);
    align_and_clamp(
        pointer_x,
        pointer_y,
        pointer_x + cw as i32,
        pointer_y + ch as i32,
        frame_w,
        frame_h,
    )
}

/// Builds the dirty_hint that the DXGI capture backend returns to the
/// upstream YUV pipeline. The hint is *advisory* — downstream uses it
/// to short-circuit per-region YUV updates and falls back to a full
/// conversion when the hint is `None`. The persistent render target's
/// composition is decided by the caller's `copy_move` + `compose_dirty`
/// invocations and is unaffected by anything this function returns.
///
/// Returns `None` when:
///
/// - the total rect count (moves + dirties + cursor delta) exceeds
///   `MAX_DIRTY_HINT_RECTS` (fragmentation fallback), or
/// - `cursor_after.visible == true` but `cursor_after_shape_known ==
///   false` (DXGI signalled a cursor present but we have not yet
///   received its shape) — without a known rect we cannot describe
///   what regions need YUV refresh.
///
/// Otherwise returns `Some(rects)` containing:
///
/// - every move's `DestinationRect` (the source rect is irrelevant
///   for YUV — the destination is where new pixels will be visible),
/// - every dirty rect,
/// - cursor delta rects per the cursor-state-transition table below.
///
/// Cursor delta transitions:
///
/// | before.visible | after.visible | rects appended |
/// |----------------|---------------|----------------|
/// | false          | false         | (none)         |
/// | false          | true          | after.rect     |
/// | true           | false         | before.rect    |
/// | true           | true (same)   | (none)         |
/// | true           | true (moved/resized) | before.rect, after.rect |
pub fn build_dirty_hint(
    moves: &[DXGI_OUTDUPL_MOVE_RECT],
    dirties: &[RECT],
    cursor_before: CursorState,
    cursor_after: CursorState,
    cursor_after_shape_known: bool,
    frame_w: u32,
    frame_h: u32,
) -> Option<Vec<DirtyRect>> {
    if cursor_after.visible && !cursor_after_shape_known {
        return None;
    }

    let cursor_delta = match (cursor_before.visible, cursor_after.visible) {
        (true, false) => 1,
        (false, true) => 1,
        (true, true) if cursor_before.rect != cursor_after.rect => 2,
        _ => 0,
    };
    let total = moves.len() + dirties.len() + cursor_delta;
    if total > MAX_DIRTY_HINT_RECTS {
        return None;
    }

    let mut hint: Vec<DirtyRect> = Vec::with_capacity(total);
    for mv in moves {
        let d = mv.DestinationRect;
        hint.push(align_and_clamp(
            d.left, d.top, d.right, d.bottom, frame_w, frame_h,
        ));
    }
    for d in dirties {
        hint.push(align_and_clamp(
            d.left, d.top, d.right, d.bottom, frame_w, frame_h,
        ));
    }
    match (cursor_before.visible, cursor_after.visible) {
        (true, false) => hint.push(cursor_before.rect),
        (false, true) => hint.push(cursor_after.rect),
        (true, true) if cursor_before.rect != cursor_after.rect => {
            hint.push(cursor_before.rect);
            hint.push(cursor_after.rect);
        }
        _ => {}
    }
    Some(hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_capture::dxgi_capture::{
        POINTER_SHAPE_TYPE_COLOR, POINTER_SHAPE_TYPE_MASKED_COLOR,
    };
    use windows::Win32::Foundation::POINT;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    fn dirty(x: u32, y: u32, width: u32, height: u32) -> DirtyRect {
        DirtyRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn dirty_rect_to_vertices_identity_basic() {
        // 1920x1080 RT, dirty rect (100, 200) - (300, 400)
        let verts = dirty_rect_to_vertices(rect(100, 200, 300, 400), 1920, 1080, 1920, 1080);

        // center_x = 960, center_y = 540
        // x0 = (100 - 960)/960 ≈ -0.895833, x1 = (300 - 960)/960 ≈ -0.6875
        // y_bot = -(400 - 540)/540 ≈ 0.259259  (D3D bottom is below center → +NDC)
        // y_top = -(200 - 540)/540 ≈ 0.629629
        // u0 = 100/1920 ≈ 0.052083, u1 = 300/1920 ≈ 0.15625
        // v_top = 200/1080 ≈ 0.185185, v_bot = 400/1080 ≈ 0.37037
        let x0 = (100.0 - 960.0) / 960.0;
        let x1 = (300.0 - 960.0) / 960.0;
        let y_bot = -(400.0 - 540.0) / 540.0;
        let y_top = -(200.0 - 540.0) / 540.0;
        let u0 = 100.0 / 1920.0;
        let u1 = 300.0 / 1920.0;
        let v_top = 200.0 / 1080.0;
        let v_bot = 400.0 / 1080.0;

        // bottom_left
        assert!(approx_eq(verts[0].pos.x, x0));
        assert!(approx_eq(verts[0].pos.y, y_bot));
        assert!(approx_eq(verts[0].tex_coord.x, u0));
        assert!(approx_eq(verts[0].tex_coord.y, v_bot));
        // top_left
        assert!(approx_eq(verts[1].pos.x, x0));
        assert!(approx_eq(verts[1].pos.y, y_top));
        assert!(approx_eq(verts[1].tex_coord.x, u0));
        assert!(approx_eq(verts[1].tex_coord.y, v_top));
        // bottom_right
        assert!(approx_eq(verts[2].pos.x, x1));
        assert!(approx_eq(verts[2].pos.y, y_bot));
        assert!(approx_eq(verts[2].tex_coord.x, u1));
        assert!(approx_eq(verts[2].tex_coord.y, v_bot));
        // Triangle 2 reuses vertices 2 and 1, then adds top_right.
        assert_eq!(verts[3], verts[2]);
        assert_eq!(verts[4], verts[1]);
        assert!(approx_eq(verts[5].pos.x, x1));
        assert!(approx_eq(verts[5].pos.y, y_top));
        assert!(approx_eq(verts[5].tex_coord.x, u1));
        assert!(approx_eq(verts[5].tex_coord.y, v_top));
    }

    #[test]
    fn dirty_rect_to_vertices_full_screen_maps_to_ndc_corners() {
        // dirty == full screen → NDC corners at ±1, UV corners at 0/1.
        let verts = dirty_rect_to_vertices(rect(0, 0, 1920, 1080), 1920, 1080, 1920, 1080);
        // bottom_left @ (-1, -1) UV (0, 1)
        assert!(approx_eq(verts[0].pos.x, -1.0));
        assert!(approx_eq(verts[0].pos.y, -1.0));
        assert!(approx_eq(verts[0].tex_coord.x, 0.0));
        assert!(approx_eq(verts[0].tex_coord.y, 1.0));
        // top_right @ (+1, +1) UV (1, 0)
        assert!(approx_eq(verts[5].pos.x, 1.0));
        assert!(approx_eq(verts[5].pos.y, 1.0));
        assert!(approx_eq(verts[5].tex_coord.x, 1.0));
        assert!(approx_eq(verts[5].tex_coord.y, 0.0));
    }

    #[test]
    fn set_move_rect_identity() {
        let mv = DXGI_OUTDUPL_MOVE_RECT {
            SourcePoint: POINT { x: 50, y: 60 },
            DestinationRect: rect(100, 120, 200, 220),
        };
        let (src, dst) = set_move_rect(&mv);
        assert_eq!(src, rect(50, 60, 150, 160)); // width 100, height 100 from dst
        assert_eq!(dst, rect(100, 120, 200, 220));
    }

    #[test]
    fn parse_move_rects_from_bytes() {
        let mv0 = DXGI_OUTDUPL_MOVE_RECT {
            SourcePoint: POINT { x: 1, y: 2 },
            DestinationRect: rect(3, 4, 5, 6),
        };
        let mv1 = DXGI_OUTDUPL_MOVE_RECT {
            SourcePoint: POINT { x: 10, y: 20 },
            DestinationRect: rect(30, 40, 50, 60),
        };
        let stride = std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
        let mut bytes = vec![0u8; stride * 2];
        unsafe {
            std::ptr::copy_nonoverlapping(
                &mv0 as *const _ as *const u8,
                bytes.as_mut_ptr(),
                stride,
            );
            std::ptr::copy_nonoverlapping(
                &mv1 as *const _ as *const u8,
                bytes.as_mut_ptr().add(stride),
                stride,
            );
        }
        let parsed = parse_move_rects(&bytes);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].SourcePoint.x, 1);
        assert_eq!(parsed[0].SourcePoint.y, 2);
        assert_eq!(parsed[0].DestinationRect, rect(3, 4, 5, 6));
        assert_eq!(parsed[1].DestinationRect, rect(30, 40, 50, 60));
    }

    #[test]
    fn parse_move_rects_truncates_malformed_remainder() {
        let stride = std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
        // One full entry + 7 garbage bytes → only the first entry survives.
        let mut bytes = vec![0u8; stride + 7];
        let mv = DXGI_OUTDUPL_MOVE_RECT {
            SourcePoint: POINT { x: 7, y: 8 },
            DestinationRect: rect(9, 10, 11, 12),
        };
        unsafe {
            std::ptr::copy_nonoverlapping(&mv as *const _ as *const u8, bytes.as_mut_ptr(), stride);
        }
        let parsed = parse_move_rects(&bytes);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].SourcePoint.x, 7);
    }

    #[test]
    fn parse_dirty_rects_from_bytes_with_remainder() {
        let stride = std::mem::size_of::<RECT>();
        let r0 = rect(1, 2, 3, 4);
        let r1 = rect(5, 6, 7, 8);
        let mut bytes = vec![0u8; stride * 2 + 3];
        unsafe {
            std::ptr::copy_nonoverlapping(&r0 as *const _ as *const u8, bytes.as_mut_ptr(), stride);
            std::ptr::copy_nonoverlapping(
                &r1 as *const _ as *const u8,
                bytes.as_mut_ptr().add(stride),
                stride,
            );
        }
        let parsed = parse_dirty_rects(&bytes);
        assert_eq!(parsed, vec![r0, r1]);
    }

    #[test]
    fn cursor_display_size_monochrome_halves_height() {
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        info.Type = POINTER_SHAPE_TYPE_MONOCHROME;
        info.Width = 32;
        info.Height = 64; // AND mask + XOR mask stacked
        assert_eq!(cursor_display_size(&info), (32, 32));
    }

    #[test]
    fn cursor_display_size_color_keeps_height() {
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        info.Type = POINTER_SHAPE_TYPE_COLOR;
        info.Width = 32;
        info.Height = 32;
        assert_eq!(cursor_display_size(&info), (32, 32));
    }

    #[test]
    fn cursor_display_size_masked_color_keeps_height() {
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        info.Type = POINTER_SHAPE_TYPE_MASKED_COLOR;
        info.Width = 16;
        info.Height = 16;
        assert_eq!(cursor_display_size(&info), (16, 16));
    }

    fn make_move(src: (i32, i32), dst: RECT) -> DXGI_OUTDUPL_MOVE_RECT {
        DXGI_OUTDUPL_MOVE_RECT {
            SourcePoint: POINT { x: src.0, y: src.1 },
            DestinationRect: dst,
        }
    }

    #[test]
    fn build_dirty_hint_move_destination_included() {
        let mv = make_move((0, 0), rect(100, 100, 300, 300));
        let hint = build_dirty_hint(
            &[mv],
            &[],
            CursorState::default(),
            CursorState::default(),
            false,
            1920,
            1080,
        )
        .expect("hint should be Some");
        assert_eq!(hint, vec![dirty(100, 100, 200, 200)]);
    }

    #[test]
    fn build_dirty_hint_dirty_rect_included() {
        let d = rect(10, 20, 110, 120);
        let hint = build_dirty_hint(
            &[],
            &[d],
            CursorState::default(),
            CursorState::default(),
            false,
            1920,
            1080,
        )
        .expect("hint should be Some");
        assert_eq!(hint, vec![dirty(10, 20, 100, 100)]);
    }

    #[test]
    fn build_dirty_hint_cursor_moved_appends_old_and_new() {
        let before = CursorState {
            visible: true,
            rect: dirty(100, 100, 32, 32),
        };
        let after = CursorState {
            visible: true,
            rect: dirty(200, 200, 32, 32),
        };
        let hint = build_dirty_hint(&[], &[], before, after, true, 1920, 1080)
            .expect("hint should be Some");
        assert_eq!(hint, vec![before.rect, after.rect]);
    }

    #[test]
    fn build_dirty_hint_cursor_hidden_emits_old_rect() {
        let before = CursorState {
            visible: true,
            rect: dirty(100, 100, 32, 32),
        };
        let after = CursorState::default();
        let hint = build_dirty_hint(&[], &[], before, after, true, 1920, 1080)
            .expect("hint should be Some");
        assert_eq!(hint, vec![before.rect]);
    }

    #[test]
    fn build_dirty_hint_cursor_first_appearance_emits_new_rect() {
        let before = CursorState::default();
        let after = CursorState {
            visible: true,
            rect: dirty(200, 200, 32, 32),
        };
        let hint = build_dirty_hint(&[], &[], before, after, true, 1920, 1080)
            .expect("hint should be Some");
        assert_eq!(hint, vec![after.rect]);
    }

    #[test]
    fn build_dirty_hint_cursor_shape_changed_emits_both() {
        // Position the same but width changes — treat as resize.
        let before = CursorState {
            visible: true,
            rect: dirty(100, 100, 32, 32),
        };
        let after = CursorState {
            visible: true,
            rect: dirty(100, 100, 64, 64),
        };
        let hint = build_dirty_hint(&[], &[], before, after, true, 1920, 1080)
            .expect("hint should be Some");
        assert_eq!(hint, vec![before.rect, after.rect]);
    }

    #[test]
    fn build_dirty_hint_cursor_static_emits_nothing_extra() {
        let pos = CursorState {
            visible: true,
            rect: dirty(100, 100, 32, 32),
        };
        let hint =
            build_dirty_hint(&[], &[], pos, pos, true, 1920, 1080).expect("hint should be Some");
        assert!(hint.is_empty());
    }

    #[test]
    fn build_dirty_hint_cursor_unknown_shape_returns_none() {
        let before = CursorState::default();
        let after = CursorState {
            visible: true,
            rect: dirty(0, 0, 0, 0),
        };
        let hint = build_dirty_hint(&[], &[], before, after, false, 1920, 1080);
        assert!(hint.is_none());
    }

    #[test]
    fn build_dirty_hint_fragmentation_returns_none() {
        // 65 dirty rects → over the cap → None.
        let dirties: Vec<RECT> = (0..65).map(|i| rect(i, i, i + 4, i + 4)).collect();
        let hint = build_dirty_hint(
            &[],
            &dirties,
            CursorState::default(),
            CursorState::default(),
            false,
            1920,
            1080,
        );
        assert!(hint.is_none());
    }

    #[test]
    fn build_dirty_hint_cap_inclusive_at_64() {
        // Exactly 64 rects must pass through.
        let dirties: Vec<RECT> = (0..64).map(|i| rect(i, i, i + 4, i + 4)).collect();
        let hint = build_dirty_hint(
            &[],
            &dirties,
            CursorState::default(),
            CursorState::default(),
            false,
            1920,
            1080,
        )
        .expect("hint should be Some at the cap");
        assert_eq!(hint.len(), 64);
    }

    #[test]
    fn cursor_rect_from_state_uses_display_size() {
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        info.Type = POINTER_SHAPE_TYPE_MONOCHROME;
        info.Width = 16;
        info.Height = 32; // mono → display height = 16
        let r = cursor_rect_from_state(100, 200, &info, 1920, 1080);
        // align_and_clamp on (100, 200, 116, 216) — all even → unchanged.
        assert_eq!(r, dirty(100, 200, 16, 16));
    }
}
