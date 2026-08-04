//! Provider-neutral validation and budgets for model image inputs.
//!
//! Images use a separate decoded-byte budget from text evidence. Callers validate
//! before persistence and again at the provider seam; the encoded-length check
//! runs before decoding so an oversized base64 string cannot force an oversized
//! allocation.

use base64::Engine as _;
use desk_agent_protocol::remote_tool::RemoteToolImage;

use crate::chat::ChatMessage;

pub const MAX_IMAGE_DECODED_BYTES: usize = 400_000;
pub const MAX_IMAGES_PER_REQUEST: usize = 4;
pub const MAX_REQUEST_IMAGE_DECODED_BYTES: usize = MAX_IMAGE_DECODED_BYTES * MAX_IMAGES_PER_REQUEST;

pub const ALLOWED_IMAGE_MEDIA_TYPES: [&str; 3] = ["image/jpeg", "image/png", "image/webp"];
pub const IMAGE_NOT_RETAINED_PLACEHOLDER: &str =
    "[image sent to the model during the active turn; original not retained]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageDataUrlInfo {
    pub media_type: String,
    pub decoded_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageInputError {
    InvalidDataUrl,
    UnsupportedMediaType,
    InvalidBase64,
    EmptyImage,
    ImageTooLarge,
    TooManyImages,
    RequestTooLarge,
    MetadataMismatch,
    InvalidDimensions,
}

impl std::fmt::Display for ImageInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidDataUrl => "invalid image data URL",
            Self::UnsupportedMediaType => "unsupported image media type",
            Self::InvalidBase64 => "invalid image base64 payload",
            Self::EmptyImage => "image payload is empty",
            Self::ImageTooLarge => "image exceeds the decoded-byte limit",
            Self::TooManyImages => "request contains too many images",
            Self::RequestTooLarge => "request images exceed the decoded-byte limit",
            Self::MetadataMismatch => "image metadata does not match its data URL",
            Self::InvalidDimensions => "image dimensions must be non-zero",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ImageInputError {}

fn max_encoded_len(max_decoded_bytes: usize) -> usize {
    max_decoded_bytes.div_ceil(3).saturating_mul(4)
}

/// Validate one canonical `data:image/...;base64,...` URL and return its trusted
/// media type and decoded size.
pub fn validate_image_data_url(url: &str) -> Result<ImageDataUrlInfo, ImageInputError> {
    let (header, payload) = url.split_once(',').ok_or(ImageInputError::InvalidDataUrl)?;
    let media_type = header
        .strip_prefix("data:")
        .and_then(|value| value.strip_suffix(";base64"))
        .ok_or(ImageInputError::InvalidDataUrl)?;
    if !ALLOWED_IMAGE_MEDIA_TYPES.contains(&media_type) {
        return Err(ImageInputError::UnsupportedMediaType);
    }
    if payload.is_empty() {
        return Err(ImageInputError::EmptyImage);
    }
    if payload.len() > max_encoded_len(MAX_IMAGE_DECODED_BYTES) {
        return Err(ImageInputError::ImageTooLarge);
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|_| ImageInputError::InvalidBase64)?;
    if decoded.is_empty() {
        return Err(ImageInputError::EmptyImage);
    }
    if decoded.len() > MAX_IMAGE_DECODED_BYTES {
        return Err(ImageInputError::ImageTooLarge);
    }
    Ok(ImageDataUrlInfo {
        media_type: media_type.to_string(),
        decoded_bytes: decoded.len(),
    })
}

/// Validate request-level count and total decoded bytes.
pub fn validate_image_request<'a>(
    urls: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<ImageDataUrlInfo>, ImageInputError> {
    let mut infos = Vec::new();
    let mut total = 0usize;
    for url in urls {
        if infos.len() == MAX_IMAGES_PER_REQUEST {
            return Err(ImageInputError::TooManyImages);
        }
        let info = validate_image_data_url(url)?;
        total = total
            .checked_add(info.decoded_bytes)
            .ok_or(ImageInputError::RequestTooLarge)?;
        if total > MAX_REQUEST_IMAGE_DECODED_BYTES {
            return Err(ImageInputError::RequestTooLarge);
        }
        infos.push(info);
    }
    Ok(infos)
}

/// Validate the redundant metadata carried on the remote-tool wire.
pub fn validate_remote_tool_image(image: &RemoteToolImage) -> Result<(), ImageInputError> {
    if image.width == 0 || image.height == 0 {
        return Err(ImageInputError::InvalidDimensions);
    }
    let info = validate_image_data_url(&image.data_url)?;
    if info.media_type != image.media_type || info.decoded_bytes != image.decoded_bytes {
        return Err(ImageInputError::MetadataMismatch);
    }
    Ok(())
}

fn remove_image(message: &mut ChatMessage) {
    if message.image_data_url.take().is_some()
        && !message.text.contains(IMAGE_NOT_RETAINED_PLACEHOLDER)
    {
        if !message.text.is_empty() {
            message.text.push('\n');
        }
        message.text.push_str(IMAGE_NOT_RETAINED_PLACEHOLDER);
    }
}

/// Validate the newest image and replace every older image with a bounded text
/// placeholder. Agent sessions therefore carry at most one data URL, and only
/// while the current turn is active. An invalid newest image is an invariant
/// violation: return an error without silently dropping it; the loop's terminal
/// cleanup strips every image before persisting the failed turn.
pub fn retain_latest_session_image(messages: &mut [ChatMessage]) -> Result<(), ImageInputError> {
    let latest = messages
        .iter()
        .rposition(|message| message.image_data_url.is_some());
    if let Some(index) = latest {
        validate_image_data_url(
            messages[index]
                .image_data_url
                .as_deref()
                .unwrap_or_default(),
        )?;
        for (message_index, message) in messages.iter_mut().enumerate() {
            if message_index != index {
                remove_image(message);
            }
        }
    }
    Ok(())
}

/// Remove all image data before a turn reaches a persistent settled state. The
/// message remains as safe text metadata so history and restored tabs agree about
/// what happened without redistributing the original image.
pub fn strip_session_images(messages: &mut [ChatMessage]) {
    for message in messages {
        remove_image(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_url(bytes: &[u8]) -> String {
        format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }

    #[test]
    fn valid_data_url_reports_decoded_size() {
        let info = validate_image_data_url(&data_url(&[1, 2, 3])).unwrap();
        assert_eq!(info.media_type, "image/jpeg");
        assert_eq!(info.decoded_bytes, 3);
    }

    #[test]
    fn rejects_bad_shape_mime_base64_and_empty_payload() {
        assert_eq!(
            validate_image_data_url("https://example.test/a.jpg").unwrap_err(),
            ImageInputError::InvalidDataUrl
        );
        assert_eq!(
            validate_image_data_url("data:image/gif;base64,AAAA").unwrap_err(),
            ImageInputError::UnsupportedMediaType
        );
        assert_eq!(
            validate_image_data_url("data:image/jpeg;base64,***").unwrap_err(),
            ImageInputError::InvalidBase64
        );
        assert_eq!(
            validate_image_data_url("data:image/jpeg;base64,").unwrap_err(),
            ImageInputError::EmptyImage
        );
    }

    #[test]
    fn rejects_oversized_payload_before_decode() {
        let url = format!(
            "data:image/jpeg;base64,{}",
            "A".repeat(max_encoded_len(MAX_IMAGE_DECODED_BYTES) + 1)
        );
        assert_eq!(
            validate_image_data_url(&url).unwrap_err(),
            ImageInputError::ImageTooLarge
        );
    }

    #[test]
    fn request_budget_rejects_fifth_image() {
        let url = data_url(&[1]);
        assert_eq!(
            validate_image_request(std::iter::repeat_n(url.as_str(), 5)).unwrap_err(),
            ImageInputError::TooManyImages
        );
    }

    #[test]
    fn decoded_image_and_request_boundaries_are_exact() {
        let maximum = data_url(&vec![7; MAX_IMAGE_DECODED_BYTES]);
        assert_eq!(
            validate_image_data_url(&maximum).unwrap().decoded_bytes,
            MAX_IMAGE_DECODED_BYTES
        );
        let infos = validate_image_request(std::iter::repeat_n(maximum.as_str(), 4)).unwrap();
        assert_eq!(
            infos.iter().map(|info| info.decoded_bytes).sum::<usize>(),
            MAX_REQUEST_IMAGE_DECODED_BYTES
        );

        let oversized = data_url(&vec![7; MAX_IMAGE_DECODED_BYTES + 1]);
        assert_eq!(
            validate_image_data_url(&oversized).unwrap_err(),
            ImageInputError::ImageTooLarge
        );
    }

    #[test]
    fn session_retains_only_latest_image_then_strips_it() {
        let mut messages = vec![
            ChatMessage::text("one", crate::chat::ChatRole::Tool, "first")
                .with_image(data_url(&[1])),
            ChatMessage::text("two", crate::chat::ChatRole::Tool, "second")
                .with_image(data_url(&[2])),
        ];
        retain_latest_session_image(&mut messages).unwrap();
        assert!(messages[0].image_data_url.is_none());
        assert!(messages[0].text.contains(IMAGE_NOT_RETAINED_PLACEHOLDER));
        assert!(messages[1].image_data_url.is_some());

        strip_session_images(&mut messages);
        assert!(
            messages
                .iter()
                .all(|message| message.image_data_url.is_none())
        );
        assert!(messages[1].text.contains(IMAGE_NOT_RETAINED_PLACEHOLDER));
    }

    #[test]
    fn invalid_latest_session_image_fails_closed_without_partial_mutation() {
        let mut messages = vec![
            ChatMessage::text("one", crate::chat::ChatRole::Tool, "first")
                .with_image(data_url(&[1])),
            ChatMessage::text("two", crate::chat::ChatRole::Tool, "second")
                .with_image("data:image/jpeg;base64,***"),
        ];

        assert!(retain_latest_session_image(&mut messages).is_err());
        assert!(
            messages
                .iter()
                .all(|message| message.image_data_url.is_some()),
            "validation failure must not masquerade as successful compaction"
        );

        strip_session_images(&mut messages);
        assert!(
            messages
                .iter()
                .all(|message| message.image_data_url.is_none())
        );
    }

    #[test]
    fn remote_metadata_must_match_data_url() {
        let image = RemoteToolImage {
            data_url: data_url(&[1, 2, 3]),
            media_type: "image/jpeg".into(),
            width: 10,
            height: 20,
            decoded_bytes: 4,
        };
        assert_eq!(
            validate_remote_tool_image(&image).unwrap_err(),
            ImageInputError::MetadataMismatch
        );
    }
}
