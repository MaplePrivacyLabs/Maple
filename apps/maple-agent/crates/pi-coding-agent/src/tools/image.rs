//! Images for the model: recognized by their bytes, converted to a format every
//! provider takes, and resized to fit within inline size limits.

use std::io::Cursor;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use pi_ai::Content;
use tokio::io::AsyncReadExt;

/// Enough of a file to recognize its image type.
const IMAGE_SNIFF_BYTES: usize = 4100;
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// The image type of `bytes`, for the types the read tool supports: PNG (not
/// animated), JPEG, GIF, WebP and BMP.
pub fn detect_supported_image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        // 0xF7 marks JPEG-LS, which decoders do not take.
        return (bytes.get(3) != Some(&0xf7)).then_some("image/jpeg");
    }
    if bytes.starts_with(&PNG_SIGNATURE) {
        return (is_png(bytes) && !is_animated_png(bytes)).then_some("image/png");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if bytes.starts_with(b"BM") && is_bmp(bytes) {
        return Some("image/bmp");
    }
    None
}

/// The image type of the file at `path`, from its first bytes.
pub async fn detect_supported_image_mime_type_from_file(
    path: &Path,
) -> std::io::Result<Option<&'static str>> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0u8; IMAGE_SNIFF_BYTES];
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file.read(&mut buffer[filled..]).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(detect_supported_image_mime_type(&buffer[..filled]))
}

fn read_u16_le(bytes: &[u8], at: usize) -> u32 {
    u32::from(bytes.get(at).copied().unwrap_or(0))
        | u32::from(bytes.get(at + 1).copied().unwrap_or(0)) << 8
}

fn read_u32_le(bytes: &[u8], at: usize) -> u32 {
    (0..4).fold(0, |value, index| {
        value | u32::from(bytes.get(at + index).copied().unwrap_or(0)) << (8 * index)
    })
}

fn read_u32_be(bytes: &[u8], at: usize) -> u32 {
    (0..4).fold(0, |value, index| {
        value << 8 | u32::from(bytes.get(at + index).copied().unwrap_or(0))
    })
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.len() >= 16 && read_u32_be(bytes, 8) == 13 && bytes.get(12..16) == Some(b"IHDR")
}

/// An APNG has an `acTL` chunk before its first `IDAT`.
fn is_animated_png(bytes: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= bytes.len() {
        let length = read_u32_be(bytes, offset) as usize;
        match bytes.get(offset + 4..offset + 8) {
            Some(b"acTL") => return true,
            Some(b"IDAT") => return false,
            _ => {}
        }
        let Some(next) = offset
            .checked_add(12)
            .and_then(|end| end.checked_add(length))
        else {
            return false;
        };
        if next > bytes.len() {
            return false;
        }
        offset = next;
    }
    false
}

fn is_bmp(bytes: &[u8]) -> bool {
    if bytes.len() < 26 {
        return false;
    }
    let declared_size = read_u32_le(bytes, 2);
    let pixel_offset = read_u32_le(bytes, 10);
    let dib_header_size = read_u32_le(bytes, 14);
    if declared_size != 0 && declared_size < 26 {
        return false;
    }
    if u64::from(pixel_offset) < 14 + u64::from(dib_header_size) {
        return false;
    }
    if declared_size != 0 && pixel_offset >= declared_size {
        return false;
    }
    let (planes, bits_per_pixel) = if dib_header_size == 12 {
        (read_u16_le(bytes, 22), read_u16_le(bytes, 24))
    } else if (40..=124).contains(&dib_header_size) {
        if bytes.len() < 30 {
            return false;
        }
        (read_u16_le(bytes, 26), read_u16_le(bytes, 28))
    } else {
        return false;
    };
    planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits_per_pixel)
}

/// Limits for an inline image.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageResizeOptions {
    pub max_width: u32,
    pub max_height: u32,
    /// Base64 bytes; 4.5MB leaves room under the strictest provider's 5MB.
    pub max_bytes: usize,
    pub jpeg_quality: u8,
}

impl Default for ImageResizeOptions {
    fn default() -> Self {
        Self {
            max_width: 2000,
            max_height: 2000,
            max_bytes: 4_718_592,
            jpeg_quality: 80,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResizedImage {
    /// Base64.
    pub data: String,
    pub mime_type: String,
    pub original_width: u32,
    pub original_height: u32,
    pub width: u32,
    pub height: u32,
    pub was_resized: bool,
}

/// An image ready for the model, with notes on what was done to it.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessedImage {
    /// Base64.
    pub data: String,
    pub mime_type: String,
    pub hints: Vec<String>,
}

fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_ascii_lowercase()
}

fn supported_mime_type(mime_type: &str) -> Option<&'static str> {
    match base_mime_type(mime_type).as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

/// Decode an image upright: a camera's EXIF orientation is applied.
fn decode_upright(bytes: &[u8]) -> Option<DynamicImage> {
    let mut decoder = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    Some(image)
}

fn encode_png(image: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(&image.to_rgb8())
        .ok()?;
    Some(out)
}

fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

/// Fit an image within `options`: first its dimensions, then its encoded size, trying
/// PNG and JPEG at falling qualities and then smaller sizes. `None` when it cannot fit.
pub fn resize_image(
    bytes: &[u8],
    mime_type: &str,
    options: &ImageResizeOptions,
) -> Option<ResizedImage> {
    let image = decode_upright(bytes)?;
    let (original_width, original_height) = (image.width(), image.height());
    if original_width <= options.max_width
        && original_height <= options.max_height
        && base64_len(bytes.len()) < options.max_bytes
    {
        return Some(ResizedImage {
            data: BASE64.encode(bytes),
            mime_type: mime_type.to_string(),
            original_width,
            original_height,
            width: original_width,
            height: original_height,
            was_resized: false,
        });
    }

    let (mut width, mut height) = (original_width, original_height);
    if width > options.max_width {
        height =
            (f64::from(height) * f64::from(options.max_width) / f64::from(width)).round() as u32;
        width = options.max_width;
    }
    if height > options.max_height {
        width =
            (f64::from(width) * f64::from(options.max_height) / f64::from(height)).round() as u32;
        height = options.max_height;
    }
    let (mut width, mut height) = (width.max(1), height.max(1));
    let mut qualities: Vec<u8> = Vec::new();
    for quality in [options.jpeg_quality, 85, 70, 55, 40] {
        if !qualities.contains(&quality) {
            qualities.push(quality);
        }
    }

    loop {
        let resized = image.resize_exact(width, height, FilterType::Lanczos3);
        let candidates = std::iter::once((encode_png(&resized), "image/png")).chain(
            qualities
                .iter()
                .map(|quality| (encode_jpeg(&resized, *quality), "image/jpeg")),
        );
        for (encoded, mime_type) in candidates {
            let Some(encoded) = encoded else { continue };
            if base64_len(encoded.len()) < options.max_bytes {
                return Some(ResizedImage {
                    data: BASE64.encode(encoded),
                    mime_type: mime_type.to_string(),
                    original_width,
                    original_height,
                    width,
                    height,
                    was_resized: true,
                });
            }
        }
        if width == 1 && height == 1 {
            return None;
        }
        let next_width = if width == 1 {
            1
        } else {
            ((f64::from(width) * 0.75) as u32).max(1)
        };
        let next_height = if height == 1 {
            1
        } else {
            ((f64::from(height) * 0.75) as u32).max(1)
        };
        if (next_width, next_height) == (width, height) {
            return None;
        }
        (width, height) = (next_width, next_height);
    }
}

/// A note that lets the model map coordinates on a resized image back to the original.
pub fn dimension_note(image: &ResizedImage) -> Option<String> {
    image.was_resized.then(|| {
        let scale = f64::from(image.original_width) / f64::from(image.width);
        format!(
            "[Image: original {}x{}, displayed at {}x{}. Multiply coordinates by {scale:.2} to map to original image.]",
            image.original_width, image.original_height, image.width, image.height
        )
    })
}

/// Make `bytes` an image the model can take: PNG, JPEG, GIF and WebP as they are,
/// anything else converted to PNG, and resized when `auto_resize` is on. The error is
/// the note the model gets instead of the image.
pub fn process_image(
    bytes: &[u8],
    mime_type: &str,
    auto_resize: bool,
    options: &ImageResizeOptions,
) -> Result<ProcessedImage, String> {
    let (bytes, mime_type, converted_from) =
        match supported_mime_type(mime_type) {
            Some(supported) => (bytes.to_vec(), supported, None),
            None => {
                let png = decode_upright(bytes).and_then(|image| encode_png(&image)).ok_or_else(|| {
                "[Image omitted: could not be converted to a supported inline image format.]"
                    .to_string()
            })?;
                (png, "image/png", Some(base_mime_type(mime_type)))
            }
        };
    let conversion_hint = |to: &str| {
        converted_from
            .as_ref()
            .filter(|from| from.as_str() != to)
            .map(|from| format!("[Image converted from {from} to {to}.]"))
    };

    if auto_resize {
        let resized = resize_image(&bytes, mime_type, options).ok_or_else(|| {
            "[Image omitted: could not be resized below the inline image size limit.]".to_string()
        })?;
        let hints = conversion_hint(&resized.mime_type)
            .into_iter()
            .chain(dimension_note(&resized))
            .collect();
        return Ok(ProcessedImage {
            data: resized.data,
            mime_type: resized.mime_type,
            hints,
        });
    }
    Ok(ProcessedImage {
        data: BASE64.encode(&bytes),
        mime_type: mime_type.to_string(),
        hints: conversion_hint(mime_type).into_iter().collect(),
    })
}

/// The images in a tool's result made to fit inline limits, as `read` makes its own.
/// Tools that make images themselves (extensions, screenshots, MCP servers) hand back
/// whatever they have, and an image a provider rejects fails every later request, so
/// they are processed once, as they enter the conversation. An image that cannot be
/// processed is kept as it is. `None` when nothing changed.
pub async fn normalize_tool_result_images(
    content: &[Content],
    auto_resize: bool,
) -> Option<Vec<Content>> {
    if !content
        .iter()
        .any(|block| matches!(block, Content::Image(_)))
    {
        return None;
    }
    let content = content.to_vec();
    tokio::task::spawn_blocking(move || normalize_images(&content, auto_resize))
        .await
        .ok()
        .flatten()
}

fn normalize_images(content: &[Content], auto_resize: bool) -> Option<Vec<Content>> {
    let options = ImageResizeOptions::default();
    let mut changed = false;
    let mut normalized = Vec::with_capacity(content.len());
    for block in content {
        let Content::Image(image) = block else {
            normalized.push(block.clone());
            continue;
        };
        let processed = BASE64
            .decode(&image.data)
            .map_err(|error| error.to_string())
            .and_then(|bytes| process_image(&bytes, &image.mime_type, auto_resize, &options));
        match processed {
            Ok(processed)
                if processed.data != image.data
                    || processed.mime_type != image.mime_type
                    || !processed.hints.is_empty() =>
            {
                normalized.push(Content::image(processed.data, processed.mime_type));
                if !processed.hints.is_empty() {
                    normalized.push(Content::text(processed.hints.join("\n")));
                }
                changed = true;
            }
            _ => normalized.push(block.clone()),
        }
    }
    changed.then_some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    pub(crate) const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==";

    #[tokio::test]
    async fn tool_result_images_are_made_to_fit_and_others_left_alone() {
        let small = vec![Content::text("shot"), Content::image(PNG_1X1, "image/png")];
        assert_eq!(normalize_tool_result_images(&small, true).await, None);
        assert_eq!(
            normalize_tool_result_images(&[Content::text("no images")], true).await,
            None
        );
        // What cannot be decoded is passed on as the tool gave it.
        let broken = vec![Content::image("not base64!", "image/png")];
        assert_eq!(normalize_tool_result_images(&broken, true).await, None);

        let wide = DynamicImage::ImageRgb8(RgbImage::from_pixel(3000, 10, Rgb([9, 9, 9])));
        let data = BASE64.encode(encode_png(&wide).unwrap());
        let normalized = normalize_tool_result_images(&[Content::image(data, "image/png")], true)
            .await
            .unwrap();
        let [Content::Image(image), Content::Text(hint)] = normalized.as_slice() else {
            panic!("{normalized:?}");
        };
        let resized = image::load_from_memory(&BASE64.decode(&image.data).unwrap()).unwrap();
        assert_eq!(resized.width(), 2000);
        assert!(
            hint.text.starts_with("[Image: original 3000x10"),
            "{}",
            hint.text
        );
    }

    pub(crate) fn tiny_bmp() -> Vec<u8> {
        let mut bytes = vec![0u8; 58];
        bytes[..2].copy_from_slice(b"BM");
        bytes[2..6].copy_from_slice(&58u32.to_le_bytes());
        bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
        bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
        bytes[18..22].copy_from_slice(&1i32.to_le_bytes());
        bytes[22..26].copy_from_slice(&1i32.to_le_bytes());
        bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
        bytes[28..30].copy_from_slice(&24u16.to_le_bytes());
        bytes[34..38].copy_from_slice(&4u32.to_le_bytes());
        bytes[56] = 0xff;
        bytes
    }

    #[test]
    fn image_types_are_recognized_by_their_bytes() {
        let png = BASE64.decode(PNG_1X1).unwrap();
        assert_eq!(detect_supported_image_mime_type(&png), Some("image/png"));
        assert_eq!(
            detect_supported_image_mime_type(&tiny_bmp()),
            Some("image/bmp")
        );
        assert_eq!(
            detect_supported_image_mime_type(b"GIF89a...."),
            Some("image/gif")
        );
        assert_eq!(
            detect_supported_image_mime_type(b"RIFF\0\0\0\0WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(
            detect_supported_image_mime_type(&[0xff, 0xd8, 0xff, 0xe0]),
            Some("image/jpeg")
        );
        assert_eq!(
            detect_supported_image_mime_type(&[0xff, 0xd8, 0xff, 0xf7]),
            None
        );
        assert_eq!(
            detect_supported_image_mime_type(b"definitely not a png"),
            None
        );
    }

    #[test]
    fn a_bmp_is_converted_to_png() {
        let processed = process_image(
            &tiny_bmp(),
            "image/bmp",
            true,
            &ImageResizeOptions::default(),
        )
        .unwrap();
        assert_eq!(processed.mime_type, "image/png");
        assert_eq!(
            processed.hints,
            ["[Image converted from image/bmp to image/png.]"]
        );
        assert_eq!(BASE64.decode(processed.data).unwrap()[0], 0x89);
    }

    #[test]
    fn a_large_image_is_resized_and_says_how_to_map_coordinates() {
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(300, 100, Rgb([200, 10, 10])));
        let png = encode_png(&image).unwrap();
        let options = ImageResizeOptions {
            max_width: 150,
            ..ImageResizeOptions::default()
        };
        let processed = process_image(&png, "image/png", true, &options).unwrap();
        assert_eq!(
            processed.hints,
            [
                "[Image: original 300x100, displayed at 150x50. Multiply coordinates by 2.00 to map to original image.]"
            ]
        );
        let decoded = image::load_from_memory(&BASE64.decode(processed.data).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (150, 50));
    }

    #[test]
    fn an_image_within_the_limits_is_sent_unchanged() {
        let png = BASE64.decode(PNG_1X1).unwrap();
        let processed =
            process_image(&png, "image/png", true, &ImageResizeOptions::default()).unwrap();
        assert_eq!(processed.data, PNG_1X1);
        assert!(processed.hints.is_empty());
    }
}
