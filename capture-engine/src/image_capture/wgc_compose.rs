//! Pure helpers backing the Windows.Graphics.Capture (WGC) pipeline.
//! Lives in its own module so the non-D3D logic can be unit-tested
//! without a graphics device. Every function here takes POD inputs
//! and returns POD outputs.

use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

/// Returns the descriptor for the CPU-readable staging texture that
/// holds the most recent WGC frame before it is mapped for read-back.
/// Format mirrors the BGRA surface format we ask the WGC frame pool
/// for, so `CopyResource` is layout-compatible.
pub fn staging_texture_desc(width: u32, height: u32) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    }
}

/// Returns true if the incoming WGC frame's content size no longer
/// matches the staging texture we previously allocated, signalling
/// that the frame pool and staging need to be recreated.
pub fn frame_needs_resize(content: SizeInt32, current: (u32, u32)) -> bool {
    if content.Width <= 0 || content.Height <= 0 {
        return false;
    }
    let w = content.Width as u32;
    let h = content.Height as u32;
    w != current.0 || h != current.1
}

/// DXGI monochrome cursor shape buffers concatenate the AND mask and
/// XOR mask vertically, so the reported `Height` is the sum of both
/// masks. The actual displayed height is `Height / 2`. Color cursors
/// use the reported height as-is.
pub fn monochrome_cursor_display_height(reported_h: u32) -> u32 {
    reported_h / 2
}

/// Repacks a 32bpp BGRA bitmap (one byte order: B, G, R, A) into
/// 32bpp RGBA (R, G, B, A) — the format expected by the cursor PNG
/// encoder. Returns `None` if `src` is too short for the requested
/// dimensions.
///
/// `src_pitch` is the byte stride of each row in `src`; rows past
/// `width * 4` are skipped (padding). Output is tightly packed.
pub fn pack_bgra_cursor(src: &[u8], width: u32, height: u32, src_pitch: u32) -> Option<Vec<u8>> {
    let row_bytes = (width as usize).checked_mul(4)?;
    let needed = (src_pitch as usize).checked_mul(height as usize)?;
    if src.len() < needed {
        return None;
    }
    let mut out = Vec::with_capacity(row_bytes * height as usize);
    for y in 0..height as usize {
        let row_start = y * src_pitch as usize;
        for x in 0..width as usize {
            let off = row_start + x * 4;
            let b = src[off];
            let g = src[off + 1];
            let r = src[off + 2];
            let a = src[off + 3];
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    Some(out)
}

/// Converts a Win32 monochrome cursor shape (AND mask + XOR mask
/// stacked vertically, 1 bit per pixel) into 32bpp RGBA. Matches the
/// rendering the DXGI backend's `process_mono_and_masked_pointer`
/// uses: `(and=1, xor=0) → transparent`, `(and=0, xor=0) → black`,
/// `(and=0, xor=1) → white`, `(and=1, xor=1) → black opaque`.
///
/// `display_height` is the actual visible height (i.e. half of the
/// raw `shape_info.Height`). Returns `None` if `src` is too short.
pub fn pack_mono_cursor(
    src: &[u8],
    width: u32,
    display_height: u32,
    pitch: u32,
) -> Option<Vec<u8>> {
    let needed = (pitch as usize).checked_mul((display_height as usize).checked_mul(2)?)?;
    if src.len() < needed {
        return None;
    }
    let mut out = Vec::with_capacity((width as usize) * (display_height as usize) * 4);
    let pitch = pitch as usize;
    for y in 0..display_height as usize {
        let and_row = y * pitch;
        let xor_row = (y + display_height as usize) * pitch;
        for x in 0..width as usize {
            let byte_offset = x / 8;
            let bit = (0x80u8) >> (x % 8);
            let and_bit = (src[and_row + byte_offset] & bit) != 0;
            let xor_bit = (src[xor_row + byte_offset] & bit) != 0;
            let (r, g, b, a) = match (and_bit, xor_bit) {
                (true, false) => (0, 0, 0, 0),
                (false, false) => (0, 0, 0, 255),
                (false, true) => (255, 255, 255, 255),
                (true, true) => (0, 0, 0, 255),
            };
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_texture_desc_matches_bgra_staging_layout() {
        let d = staging_texture_desc(1920, 1080);
        assert_eq!(d.Width, 1920);
        assert_eq!(d.Height, 1080);
        assert_eq!(d.MipLevels, 1);
        assert_eq!(d.ArraySize, 1);
        assert_eq!(d.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
        assert_eq!(d.SampleDesc.Count, 1);
        assert_eq!(d.SampleDesc.Quality, 0);
        assert_eq!(d.Usage, D3D11_USAGE_STAGING);
        assert_eq!(d.BindFlags, 0);
        assert_eq!(d.CPUAccessFlags, D3D11_CPU_ACCESS_READ.0 as u32);
        assert_eq!(d.MiscFlags, 0);
    }

    #[test]
    fn frame_needs_resize_detects_change() {
        assert!(!frame_needs_resize(
            SizeInt32 {
                Width: 1920,
                Height: 1080
            },
            (1920, 1080)
        ));
        assert!(frame_needs_resize(
            SizeInt32 {
                Width: 2560,
                Height: 1080
            },
            (1920, 1080)
        ));
        assert!(frame_needs_resize(
            SizeInt32 {
                Width: 1920,
                Height: 1200
            },
            (1920, 1080)
        ));
    }

    #[test]
    fn frame_needs_resize_rejects_nonpositive_content_size() {
        // WGC sometimes reports a zero/negative content size during
        // teardown; treat it as "no resize needed" rather than thrash.
        assert!(!frame_needs_resize(
            SizeInt32 {
                Width: 0,
                Height: 1080
            },
            (1920, 1080)
        ));
        assert!(!frame_needs_resize(
            SizeInt32 {
                Width: 1920,
                Height: 0
            },
            (1920, 1080)
        ));
        assert!(!frame_needs_resize(
            SizeInt32 {
                Width: -1,
                Height: 1080
            },
            (1920, 1080)
        ));
    }

    #[test]
    fn monochrome_cursor_display_height_halves() {
        assert_eq!(monochrome_cursor_display_height(64), 32);
        assert_eq!(monochrome_cursor_display_height(32), 16);
        assert_eq!(monochrome_cursor_display_height(0), 0);
        // Odd input → integer division floor; documents Win32 behavior.
        assert_eq!(monochrome_cursor_display_height(33), 16);
    }

    #[test]
    fn pack_bgra_cursor_swaps_channels_and_skips_padding() {
        // 2x1 cursor, 12-byte pitch (4 bytes padding at end of row)
        let src: Vec<u8> = vec![
            0x11, 0x22, 0x33, 0x44, // pixel 0: B=11 G=22 R=33 A=44
            0x55, 0x66, 0x77, 0x88, // pixel 1: B=55 G=66 R=77 A=88
            0xFF, 0xFF, 0xFF, 0xFF, // padding (skipped)
        ];
        let out = pack_bgra_cursor(&src, 2, 1, 12).expect("pack ok");
        assert_eq!(out, vec![0x33, 0x22, 0x11, 0x44, 0x77, 0x66, 0x55, 0x88]);
    }

    #[test]
    fn pack_bgra_cursor_rejects_truncated_src() {
        let src = vec![0u8; 7]; // 2x1 BGRA needs 8 bytes when pitch=8
        assert!(pack_bgra_cursor(&src, 2, 1, 8).is_none());
    }

    #[test]
    fn pack_mono_cursor_produces_expected_states() {
        // 8x1 mono cursor: 1-byte pitch, 2 rows (AND then XOR)
        // AND mask = 0b1100_0000 → x=0,1 transparent; x=2..7 opaque
        // XOR mask = 0b0010_0000 → x=2 white; x=3..7 black; x=0,1 transparent
        let src = vec![0b1100_0000, 0b0010_0000];
        let out = pack_mono_cursor(&src, 8, 1, 1).expect("pack ok");
        assert_eq!(out.len(), 8 * 4);
        // x=0: and=1 xor=0 → transparent
        assert_eq!(&out[0..4], &[0, 0, 0, 0]);
        // x=1: and=1 xor=0 → transparent
        assert_eq!(&out[4..8], &[0, 0, 0, 0]);
        // x=2: and=0 xor=1 → white
        assert_eq!(&out[8..12], &[255, 255, 255, 255]);
        // x=3: and=0 xor=0 → black opaque
        assert_eq!(&out[12..16], &[0, 0, 0, 255]);
    }

    #[test]
    fn pack_mono_cursor_rejects_truncated_src() {
        // 8x1 mono needs (pitch=1) * (height*2=2) = 2 bytes
        let src = vec![0u8; 1];
        assert!(pack_mono_cursor(&src, 8, 1, 1).is_none());
    }
}
