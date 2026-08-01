use std::io::Cursor;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader};
use pi_ai::ContentBlock;

pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_DECODED_IMAGE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DECODED_BYTES_PER_PIXEL: u64 = 8;
const MAX_IMAGE_PIXELS: u64 = MAX_DECODED_IMAGE_BYTES / MAX_DECODED_BYTES_PER_PIXEL;
const MAX_WIDTH: u32 = 2_000;
const MAX_HEIGHT: u32 = 2_000;
const MAX_INLINE_BASE64_BYTES: usize = (4.5 * 1024.0 * 1024.0) as usize;
const JPEG_QUALITIES: &[u8] = &[80, 85, 70, 55, 40];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessedImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
    pub original_width: u32,
    pub original_height: u32,
    pub width: u32,
    pub height: u32,
    pub was_resized: bool,
}

impl ProcessedImage {
    #[must_use]
    pub fn into_content_block(self) -> ContentBlock {
        ContentBlock::Image {
            data: base64::engine::general_purpose::STANDARD.encode(self.bytes),
            mime_type: self.mime_type,
        }
    }

    #[must_use]
    pub fn dimension_hint(&self) -> Option<String> {
        if !self.was_resized || self.width == 0 {
            return None;
        }
        let scale = f64::from(self.original_width) / f64::from(self.width);
        Some(format!(
            "[Image: original {}x{}, displayed at {}x{}. Multiply coordinates by {scale:.2} to map to original image.]",
            self.original_width, self.original_height, self.width, self.height
        ))
    }
}

#[must_use]
pub fn supported_mime(bytes: &[u8]) -> Option<&'static str> {
    image::guess_format(bytes).ok().and_then(format_mime)
}

pub fn validate_image(bytes: &[u8], advertised_mime: &str) -> Result<()> {
    validate_image_data(bytes, Some(advertised_mime)).map(|_| ())
}

fn validate_image_data(
    bytes: &[u8],
    advertised_mime: Option<&str>,
) -> Result<(&'static str, ImageFormat, u32, u32)> {
    if bytes.is_empty() {
        bail!("image is empty");
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!(
            "image exceeds the {} MiB limit",
            MAX_IMAGE_BYTES / 1024 / 1024
        );
    }

    let actual_mime =
        supported_mime(bytes).ok_or_else(|| anyhow!("data is not a supported image"))?;
    let format = image::guess_format(bytes).context("data is not a supported image")?;
    if let Some(advertised_mime) = advertised_mime {
        let advertised_mime = base_mime(advertised_mime);
        if advertised_mime != actual_mime {
            bail!("image advertised {advertised_mime}, but the data is {actual_mime}");
        }
    }

    let (width, height) = image_dimensions(bytes, format)?;
    validate_dimensions(width, height)?;
    Ok((actual_mime, format, width, height))
}

pub fn process_image(bytes: Vec<u8>, advertised_mime: Option<&str>) -> Result<ProcessedImage> {
    let (actual_mime, format, original_width, original_height) =
        validate_image_data(&bytes, advertised_mime)?;
    let decoded = decode_image(&bytes, format)?;
    if original_width <= MAX_WIDTH
        && original_height <= MAX_HEIGHT
        && base64_size(bytes.len()) < MAX_INLINE_BASE64_BYTES
    {
        return Ok(ProcessedImage {
            bytes,
            mime_type: actual_mime.to_owned(),
            original_width,
            original_height,
            width: original_width,
            height: original_height,
            was_resized: false,
        });
    }

    let (mut target_width, mut target_height) = (original_width, original_height);
    if target_width > MAX_WIDTH {
        target_height =
            js_round(f64::from(target_height) * f64::from(MAX_WIDTH) / f64::from(target_width));
        target_width = MAX_WIDTH;
    }
    if target_height > MAX_HEIGHT {
        target_width =
            js_round(f64::from(target_width) * f64::from(MAX_HEIGHT) / f64::from(target_height));
        target_height = MAX_HEIGHT;
    }
    target_width = target_width.max(1);
    target_height = target_height.max(1);

    let (mut width, mut height) = (target_width, target_height);
    loop {
        let resized = decoded.resize_exact(width, height, FilterType::Triangle);
        if let Some((encoded, mime_type)) = encode_under_limit(&resized) {
            return Ok(ProcessedImage {
                bytes: encoded,
                mime_type,
                original_width,
                original_height,
                width,
                height,
                was_resized: true,
            });
        }
        if width == 1 && height == 1 {
            break;
        }
        let next_width = if width == 1 {
            1
        } else {
            ((f64::from(width) * 0.75).floor() as u32).max(1)
        };
        let next_height = if height == 1 {
            1
        } else {
            ((f64::from(height) * 0.75).floor() as u32).max(1)
        };
        if next_width == width && next_height == height {
            break;
        }
        width = next_width;
        height = next_height;
    }

    bail!("image could not be resized below the inline image size limit")
}

fn image_dimensions(bytes: &[u8], format: ImageFormat) -> Result<(u32, u32)> {
    ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .context("could not read image dimensions")
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width == 0 || height == 0 || pixels > MAX_IMAGE_PIXELS {
        bail!(
            "image dimensions {width}x{height} exceed the {} MiB decoded-memory safety limit",
            MAX_DECODED_IMAGE_BYTES / 1024 / 1024
        );
    }
    Ok(())
}

fn decode_image(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODED_IMAGE_BYTES);
    reader.limits(limits);
    reader.decode().context("could not decode image")
}

fn encode_under_limit(image: &DynamicImage) -> Option<(Vec<u8>, String)> {
    let mut bytes = Vec::new();
    if image
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .is_ok()
        && base64_size(bytes.len()) < MAX_INLINE_BASE64_BYTES
    {
        return Some((bytes, "image/png".to_owned()));
    }
    for &quality in JPEG_QUALITIES {
        bytes.clear();
        if image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
            .encode_image(image)
            .is_ok()
            && base64_size(bytes.len()) < MAX_INLINE_BASE64_BYTES
        {
            return Some((bytes, "image/jpeg".to_owned()));
        }
    }
    None
}

fn format_mime(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Gif => Some("image/gif"),
        ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
}

fn base_mime(mime_type: &str) -> &str {
    mime_type.split(';').next().unwrap_or(mime_type).trim()
}

const fn base64_size(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

fn js_round(value: f64) -> u32 {
    (value + 0.5).floor() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageBuffer;

    fn encoded(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_fn(width, height, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8])
        }));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), format)
            .expect("encode image");
        bytes
    }

    #[test]
    fn supported_small_formats_are_preserved() {
        for (format, mime_type) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::Jpeg, "image/jpeg"),
            (ImageFormat::Gif, "image/gif"),
            (ImageFormat::WebP, "image/webp"),
        ] {
            let bytes = encoded(format, 2, 2);
            assert_eq!(supported_mime(&bytes), Some(mime_type));
            let processed = process_image(bytes.clone(), Some(mime_type)).expect("process image");
            assert_eq!(processed.bytes, bytes);
            assert_eq!(processed.mime_type, mime_type);
            assert!(!processed.was_resized);
        }
    }

    #[test]
    fn oversized_dimensions_are_resized_instead_of_rejected() {
        let processed =
            process_image(encoded(ImageFormat::Png, 2_100, 3), None).expect("resize valid image");
        assert!(processed.was_resized);
        assert_eq!(
            (processed.original_width, processed.original_height),
            (2_100, 3)
        );
        assert!(processed.width <= MAX_WIDTH);
        assert!(processed.height <= MAX_HEIGHT);
        assert!(matches!(
            processed.mime_type.as_str(),
            "image/png" | "image/jpeg"
        ));
    }

    #[test]
    fn decoded_memory_boundary_is_explicit() {
        validate_dimensions(4_096, 4_096).expect("16M pixels stays within 128 MiB budget");
        let error = validate_dimensions(4_097, 4_096).expect_err("image above boundary rejected");
        assert!(error.to_string().contains("128 MiB decoded-memory safety limit"));
    }

    #[test]
    fn compressed_large_image_is_rejected_before_decode() {
        let bytes = encoded(ImageFormat::Png, 4_097, 4_096);
        assert!(bytes.len() <= MAX_IMAGE_BYTES);
        let error = process_image(bytes, Some("image/png")).expect_err("decoded memory guard");
        assert!(error.to_string().contains("decoded-memory safety limit"));
    }

    #[test]
    fn advertised_mime_must_match_image_data() {
        let error = process_image(encoded(ImageFormat::Png, 2, 2), Some("image/jpeg"))
            .expect_err("mime mismatch");
        assert!(error.to_string().contains("advertised image/jpeg"));
    }
}
