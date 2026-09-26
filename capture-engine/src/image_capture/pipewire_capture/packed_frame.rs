//! Normalize mapped SPA chunks to tightly packed BGRA without reading padding.
#[derive(Clone, Copy)]
pub(super) enum Format {
    Rgb,
    Rgba,
    Rgbx,
    Bgra,
    Bgrx,
}

pub(super) fn bgra(
    format: Format,
    width: u32,
    height: u32,
    offset: u32,
    stride: i32,
    chunk_size: u32,
    mapped: &[u8],
) -> Result<Vec<u8>, &'static str> {
    let width = width as usize;
    let height = height as usize;
    let pixels = width
        .checked_mul(height)
        .ok_or("frame dimensions overflow")?;
    if width == 0 || height == 0 || pixels > 64 * 1024 * 1024 {
        return Err("frame dimensions exceed the capture bound");
    }
    let channels = if matches!(format, Format::Rgb) { 3 } else { 4 };
    let row_bytes = width.checked_mul(channels).ok_or("row size overflow")?;
    let stride = usize::try_from(stride).map_err(|_| "negative frame stride is unsupported")?;
    if stride < row_bytes {
        return Err("frame stride is shorter than a row");
    }
    let occupied = stride
        .checked_mul(height - 1)
        .and_then(|n| n.checked_add(row_bytes))
        .ok_or("frame span overflow")?;
    let start = offset as usize;
    let end = start
        .checked_add(chunk_size as usize)
        .ok_or("chunk span overflow")?;
    if occupied > chunk_size as usize || end > mapped.len() {
        return Err("frame extends outside its mapped chunk");
    }
    let source = &mapped[start..end];
    let mut output = Vec::new();
    output
        .try_reserve_exact(pixels.checked_mul(4).ok_or("output size overflow")?)
        .map_err(|_| "could not allocate packed frame")?;
    for row in 0..height {
        for pixel in source[row * stride..row * stride + row_bytes].chunks_exact(channels) {
            let bgra = match format {
                Format::Rgb | Format::Rgbx => [pixel[2], pixel[1], pixel[0], 255],
                Format::Rgba => [pixel[2], pixel[1], pixel[0], pixel[3]],
                Format::Bgra => [pixel[0], pixel[1], pixel[2], pixel[3]],
                Format::Bgrx => [pixel[0], pixel[1], pixel[2], 255],
            };
            output.extend_from_slice(&bgra);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offset_and_padding_are_excluded_and_rgbx_is_four_bytes_per_pixel() {
        let mapped = [99, 99, 1, 2, 3, 88, 77, 77, 4, 5, 6, 88, 99];
        assert_eq!(
            bgra(Format::Rgbx, 1, 2, 2, 6, 10, &mapped).unwrap(),
            [3, 2, 1, 255, 6, 5, 4, 255]
        );
        assert_eq!(
            bgra(Format::Rgb, 1, 1, 2, 3, 3, &mapped).unwrap(),
            [3, 2, 1, 255]
        );
        assert_eq!(
            bgra(Format::Rgba, 1, 1, 2, 4, 4, &mapped).unwrap(),
            [3, 2, 1, 88]
        );
        assert_eq!(
            bgra(Format::Bgra, 1, 1, 2, 4, 4, &mapped).unwrap(),
            [1, 2, 3, 88]
        );
        assert_eq!(
            bgra(Format::Bgrx, 1, 1, 2, 4, 4, &mapped).unwrap(),
            [1, 2, 3, 255]
        );
    }
    #[test]
    fn invalid_geometry_and_truncated_chunks_are_rejected() {
        for (width, height, offset, stride, length) in [
            (0, 1, 0, 4, 8),
            (1, 0, 0, 4, 8),
            (1, 1, 0, -4, 8),
            (2, 1, 0, 4, 8),
            (1, 2, 0, 4, 7),
            (1, 1, 5, 4, 4),
            (u32::MAX, u32::MAX, 0, 4, 8),
        ] {
            assert!(bgra(Format::Rgbx, width, height, offset, stride, length, &[0; 8]).is_err());
        }
    }
}
