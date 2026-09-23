use std::io::Read;

use flate2::read::ZlibDecoder;

use crate::ConversionError;

const MAX_OBJECTS: usize = 100_000;
const MAX_STREAMS: usize = 50_000;
const MAX_DICTIONARY_BYTES: usize = 64 * 1024;
const MAX_DECOMPRESSED_STREAM_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOTAL_DECOMPRESSED_BYTES: usize = 64 * 1024 * 1024;
const MAX_COMPRESSION_RATIO: usize = 256;

pub(crate) fn validate(source: &[u8]) -> Result<(), ConversionError> {
    if !source
        .get(..source.len().min(1_024))
        .is_some_and(|prefix| prefix.windows(5).any(|window| window == b"%PDF-"))
    {
        return Err(ConversionError::new(
            "source_format_mismatch",
            "source does not contain a PDF header",
        ));
    }
    if contains_token(source, b"/Encrypt") {
        return Err(ConversionError::new(
            "encrypted_pdf",
            "encrypted PDFs are not supported",
        ));
    }
    if count_token(source, b" obj") > MAX_OBJECTS {
        return Err(limit("PDF object count exceeds the supported limit"));
    }

    let mut cursor = 0usize;
    let mut stream_count = 0usize;
    let mut decompressed_total = 0usize;
    while let Some(relative) = find_token(&source[cursor..], b"stream") {
        let keyword = cursor + relative;
        if !is_stream_keyword(source, keyword) {
            cursor = keyword + 6;
            continue;
        }
        stream_count += 1;
        if stream_count > MAX_STREAMS {
            return Err(limit("PDF stream count exceeds the supported limit"));
        }
        let dictionary_start = source[..keyword]
            .windows(2)
            .rposition(|window| window == b"<<")
            .ok_or_else(|| invalid("PDF stream is missing its dictionary"))?;
        if keyword.saturating_sub(dictionary_start) > MAX_DICTIONARY_BYTES {
            return Err(limit("PDF stream dictionary exceeds the supported limit"));
        }
        let dictionary = &source[dictionary_start..keyword];
        let length = direct_length(dictionary)?;
        let data_start = stream_data_start(source, keyword + 6)?;
        let data_end = data_start
            .checked_add(length)
            .filter(|end| *end <= source.len())
            .ok_or_else(|| invalid("PDF stream length exceeds the source"))?;
        let encoded = &source[data_start..data_end];

        if dictionary.windows(7).any(|window| window == b"/Filter") {
            if dictionary.windows(2).any(|window| window == b"[/") {
                return Err(ConversionError::new(
                    "unsupported_pdf_filter",
                    "PDF streams with chained filters are not supported by the bounded parser",
                ));
            }
            if contains_token(dictionary, b"/FlateDecode") || contains_token(dictionary, b"/Fl") {
                let decoded = bounded_inflate(encoded)?;
                decompressed_total = decompressed_total
                    .checked_add(decoded)
                    .ok_or_else(|| limit("PDF decompression budget overflow"))?;
            } else if !(contains_token(dictionary, b"/DCTDecode")
                || contains_token(dictionary, b"/JPXDecode")
                || contains_token(dictionary, b"/CCITTFaxDecode"))
            {
                return Err(ConversionError::new(
                    "unsupported_pdf_filter",
                    "PDF stream uses a filter that cannot be safely bounded",
                ));
            }
        } else {
            decompressed_total = decompressed_total
                .checked_add(encoded.len())
                .ok_or_else(|| limit("PDF stream budget overflow"))?;
        }
        if decompressed_total > MAX_TOTAL_DECOMPRESSED_BYTES {
            return Err(limit("PDF decompressed streams exceed the 64 MiB budget"));
        }
        cursor = data_end;
    }
    Ok(())
}

fn bounded_inflate(encoded: &[u8]) -> Result<usize, ConversionError> {
    let ratio_limit = encoded
        .len()
        .saturating_mul(MAX_COMPRESSION_RATIO)
        .max(1_024);
    let limit = MAX_DECOMPRESSED_STREAM_BYTES.min(ratio_limit);
    let mut decoder = ZlibDecoder::new(encoded);
    let mut sink = Vec::with_capacity(limit.min(64 * 1024));
    decoder
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut sink)
        .map_err(|_| invalid("PDF contains an invalid Flate stream"))?;
    if sink.len() > limit {
        return Err(limit_error(
            "PDF stream exceeds the decompression or compression-ratio limit",
        ));
    }
    Ok(sink.len())
}

fn direct_length(dictionary: &[u8]) -> Result<usize, ConversionError> {
    let position = find_token(dictionary, b"/Length")
        .ok_or_else(|| invalid("PDF stream has no direct length"))?;
    let mut cursor = position + 7;
    while dictionary.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    let start = cursor;
    while dictionary.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if start == cursor {
        return Err(ConversionError::new(
            "unsupported_pdf_stream_length",
            "PDF stream length must be a direct bounded integer",
        ));
    }
    let number = std::str::from_utf8(&dictionary[start..cursor])
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| invalid("PDF stream length is invalid"))?;
    let mut after = cursor;
    while dictionary.get(after).is_some_and(u8::is_ascii_whitespace) {
        after += 1;
    }
    if dictionary.get(after).is_some_and(u8::is_ascii_digit) {
        return Err(ConversionError::new(
            "unsupported_pdf_stream_length",
            "indirect PDF stream lengths are not supported by the bounded parser",
        ));
    }
    Ok(number)
}

fn stream_data_start(source: &[u8], mut cursor: usize) -> Result<usize, ConversionError> {
    if source.get(cursor..cursor + 2) == Some(b"\r\n") {
        cursor += 2;
    } else if source.get(cursor) == Some(&b'\n') || source.get(cursor) == Some(&b'\r') {
        cursor += 1;
    } else {
        return Err(invalid(
            "PDF stream keyword is not followed by a line ending",
        ));
    }
    Ok(cursor)
}

fn is_stream_keyword(source: &[u8], offset: usize) -> bool {
    let before = offset.checked_sub(1).and_then(|index| source.get(index));
    let after = source.get(offset + 6);
    before.is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b'>')
        && after.is_some_and(|byte| byte.is_ascii_whitespace())
}

fn contains_token(haystack: &[u8], needle: &[u8]) -> bool {
    find_token(haystack, needle).is_some()
}

fn count_token(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn find_token(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn invalid(message: &'static str) -> ConversionError {
    ConversionError::new("invalid_pdf", message)
}

fn limit(message: &'static str) -> ConversionError {
    limit_error(message)
}

fn limit_error(message: &'static str) -> ConversionError {
    ConversionError::new("pdf_resource_limit_exceeded", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_encryption_before_parser_decryption() {
        let error = validate(b"%PDF-1.7\ntrailer << /Encrypt 2 0 R >>\n%%EOF").unwrap_err();
        assert_eq!(error.code, "encrypted_pdf");
    }

    #[test]
    fn rejects_indirect_stream_lengths() {
        let error =
            validate(b"%PDF-1.7\n1 0 obj << /Length 2 0 R >>\nstream\nabc\nendstream\nendobj")
                .unwrap_err();
        assert_eq!(error.code, "unsupported_pdf_stream_length");
    }
}
