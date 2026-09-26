//! Scoped PipeWire buffer ownership with optional SPA presentation timestamps.
use pipewire::{spa, stream::StreamRef};
use std::ptr::NonNull;

pub(super) struct Buffer<'a> {
    raw: NonNull<pipewire::sys::pw_buffer>,
    stream: &'a StreamRef,
}

impl<'a> Buffer<'a> {
    pub(super) fn dequeue(stream: &'a StreamRef) -> Option<Self> {
        // The callback exclusively borrows this dequeued buffer until Drop.
        NonNull::new(unsafe { stream.dequeue_raw_buffer() }).map(|raw| Self { raw, stream })
    }

    pub(super) fn header(&self) -> Result<Option<Header>, &'static str> {
        let buffer = unsafe { self.raw.as_ref().buffer.as_ref() }.ok_or("missing SPA buffer")?;
        if buffer.n_metas == 0 {
            return Ok(None);
        }
        if buffer.n_metas > 64 || buffer.metas.is_null() {
            return Err("invalid SPA metadata array");
        }
        // Metadata is owned by the dequeued buffer and cannot outlive this guard.
        let metas = unsafe { std::slice::from_raw_parts(buffer.metas, buffer.n_metas as usize) };
        // Each non-null metadata pointer belongs to this borrowed native buffer.
        unsafe { read_header(metas) }
    }

    pub(super) fn datas_mut(&mut self) -> &mut [spa::buffer::Data] {
        let Some(buffer) = (unsafe { self.raw.as_ref().buffer.as_mut() }) else {
            return &mut [];
        };
        if buffer.n_datas == 0 || buffer.n_datas > 64 || buffer.datas.is_null() {
            return &mut [];
        }
        // Data is repr(transparent) over spa_data in the pinned libspa version.
        unsafe { std::slice::from_raw_parts_mut(buffer.datas.cast(), buffer.n_datas as usize) }
    }
}

impl Drop for Buffer<'_> {
    fn drop(&mut self) {
        // Exactly the pointer obtained from this stream's dequeue operation.
        unsafe {
            self.stream.queue_raw_buffer(self.raw.as_ptr());
        }
    }
}

/// Non-null metadata payloads must refer to the native allocation described by size.
unsafe fn read_header(metas: &[spa::sys::spa_meta]) -> Result<Option<Header>, &'static str> {
    let mut result = None;
    for meta in metas {
        if meta.type_ != spa::sys::SPA_META_Header {
            continue;
        }
        if result.is_some() {
            return Err("duplicate SPA frame header");
        }
        if meta.size < std::mem::size_of::<spa::sys::spa_meta_header>() as u32
            || meta.data.is_null()
        {
            return Err("invalid SPA frame header");
        }
        let header =
            unsafe { std::ptr::read_unaligned(meta.data.cast::<spa::sys::spa_meta_header>()) };
        result = Some(Header::new(header.pts, header.flags));
    }
    Ok(result)
}

#[derive(Debug, Default)]
pub(super) struct Header {
    pub timestamp_ns: Option<u64>,
    pub discontinuity: bool,
    pub unusable: bool,
}
impl Header {
    fn new(pts: i64, flags: u32) -> Self {
        Self {
            timestamp_ns: valid_timestamp(pts, flags),
            discontinuity: flags & 1 != 0,
            unusable: flags & ((1 << 1) | (1 << 4)) != 0,
        }
    }
}

fn valid_timestamp(pts: i64, flags: u32) -> Option<u64> {
    // SPA_META_HEADER_FLAG_CORRUPTED and GAP do not identify an observed image.
    (pts >= 0 && flags & ((1 << 1) | (1 << 4)) == 0).then_some(pts as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absent_header_differs_from_malformed_or_duplicate_header() {
        assert!(unsafe { read_header(&[]) }.unwrap().is_none());
        let mut header: spa::sys::spa_meta_header = unsafe { std::mem::zeroed() };
        header.pts = 123;
        let valid = || spa::sys::spa_meta {
            type_: spa::sys::SPA_META_Header,
            size: std::mem::size_of::<spa::sys::spa_meta_header>() as u32,
            data: (&header as *const spa::sys::spa_meta_header)
                .cast_mut()
                .cast(),
        };
        assert_eq!(
            unsafe { read_header(&[valid()]) }
                .unwrap()
                .unwrap()
                .timestamp_ns,
            Some(123)
        );
        assert!(unsafe { read_header(&[valid(), valid()]) }.is_err());
        let mut short = valid();
        short.size -= 1;
        assert!(unsafe { read_header(&[short]) }.is_err());
        let mut missing = valid();
        missing.data = std::ptr::null_mut();
        assert!(unsafe { read_header(&[missing]) }.is_err());
    }

    #[test]
    fn discontinuity_accepts_new_pixels_while_corrupt_or_gap_headers_do_not() {
        let discontinuous = Header::new(42, 1);
        assert!(discontinuous.discontinuity);
        assert!(!discontinuous.unusable);
        assert_eq!(discontinuous.timestamp_ns, Some(42));
        for flags in [1 << 1, 1 << 4] {
            let invalid = Header::new(42, flags);
            assert!(invalid.unusable);
            assert_eq!(invalid.timestamp_ns, None);
        }
    }

    #[test]
    fn presentation_time_is_optional_and_not_a_wall_clock_conversion() {
        assert_eq!(valid_timestamp(-1, 0), None);
        assert_eq!(valid_timestamp(i64::MIN, 0), None);
        assert_eq!(valid_timestamp(42, 1 << 1), None);
        assert_eq!(valid_timestamp(42, 1 << 4), None);
        assert_eq!(valid_timestamp(0, 0), Some(0));
        assert_eq!(valid_timestamp(42, 0), Some(42));
    }
}
