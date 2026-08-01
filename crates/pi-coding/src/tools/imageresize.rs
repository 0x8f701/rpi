//! Image post-processing for the read tool (port of pi's
//! `coding/imageresize.go`, mirroring `utils/image-resize-core.ts` and
//! `utils/image-process.ts`).
//!
//! Downscales images that exceed the inline limits before sending them to the
//! model and applies EXIF orientation. The decision surface (target
//! dimensions, format choice, wasResized) is a faithful port. Pixel resizing
//! uses the image crate's high-quality Lanczos3 filter, equivalent to pi's
//! Photon/Lanczos3 path.

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageFormat};
use std::io::Cursor;

const IMG_MAX_WIDTH: u32 = 2000;
const IMG_MAX_HEIGHT: u32 = 2000;
const IMG_MAX_BASE64_BYTES: usize = (4.5 * 1024.0 * 1024.0) as usize;

/// Matches pi's `qualitySteps = dedupe([jpegQuality(default 80), 85, 70, 55, 40])`.
const JPEG_QUALITIES: &[u8] = &[80, 85, 70, 55, 40];

/// Mirrors the object pi's `resizeImage` returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResizeResult {
    /// Raw image bytes to send to the model (not base64).
    pub data: Vec<u8>,
    pub mime_type: String,
    pub original_width: u32,
    pub original_height: u32,
    pub width: u32,
    pub height: u32,
    pub was_resized: bool,
}

/// Returns the encoded length of `n` bytes — `ceil(n/3)*4`, matching pi's
/// `Math.ceil(inputBytes.byteLength / 3) * 4`.
fn base64_size(n: usize) -> usize {
    ((n + 2) / 3) * 4
}

/// Mirrors JS `Math.round` (round half toward +Infinity) for non-negative x.
fn js_round(x: f64) -> u32 {
    (x + 0.5).floor() as u32
}

/// A faithful port of pi's `resizeImageInProcess`. Returns the decision result,
/// or `None` when the image cannot be brought under the byte limit (pi returns
/// null).
pub(crate) fn resize_image(input_bytes: &[u8], mime_type: &str) -> Option<ResizeResult> {
    let input_b64 = base64_size(input_bytes.len());

    let img = image::load_from_memory(input_bytes).ok()?;
    let format = image::guess_format(input_bytes).ok()?;

    // Apply EXIF orientation to the working image (used for dimensions and the
    // resized output), reading the orientation from the original bytes.
    let oriented = apply_exif_orientation_from_bytes(&img, input_bytes);
    let (ow, oh) = oriented.dimensions();

    let mime = if mime_type.is_empty() {
        format_to_mime(format).to_string()
    } else {
        mime_type.to_string()
    };

    // Already within all limits → return the ORIGINAL bytes unchanged (pi does
    // not bake orientation here; it reports post-orientation dimensions and
    // relies on the model honoring EXIF). wasResized = false.
    if ow <= IMG_MAX_WIDTH && oh <= IMG_MAX_HEIGHT && input_b64 < IMG_MAX_BASE64_BYTES {
        return Some(ResizeResult {
            data: input_bytes.to_vec(),
            mime_type: mime,
            original_width: ow,
            original_height: oh,
            width: ow,
            height: oh,
            was_resized: false,
        });
    }

    // Initial target: scale to fit within max dimensions, preserving aspect.
    // pi uses Math.round for the dependent dimension.
    let (mut tw, mut th) = (ow, oh);
    if tw > IMG_MAX_WIDTH {
        th = js_round(th as f64 * IMG_MAX_WIDTH as f64 / tw as f64);
        tw = IMG_MAX_WIDTH;
    }
    if th > IMG_MAX_HEIGHT {
        tw = js_round(tw as f64 * IMG_MAX_HEIGHT as f64 / th as f64);
        th = IMG_MAX_HEIGHT;
    }

    // Shrink-and-encode loop: at each size try PNG then the JPEG quality steps,
    // taking the first candidate under the byte limit; otherwise scale down by
    // 0.75 (floored) until 1x1 (mirrors pi's while loop exactly).
    let (mut cw, mut ch) = (tw, th);
    loop {
        let scaled = if cw != ow || ch != oh {
            oriented.resize_exact(cw, ch, FilterType::Lanczos3)
        } else {
            oriented.clone()
        };
        if let Some((enc, mime_out)) = encode_under_limit(&scaled) {
            return Some(ResizeResult {
                data: enc,
                mime_type: mime_out,
                original_width: ow,
                original_height: oh,
                width: cw,
                height: ch,
                was_resized: true,
            });
        }
        if cw == 1 && ch == 1 {
            break;
        }
        let mut nw = cw;
        let mut nh = ch;
        if cw != 1 {
            nw = max1((cw as f64 * 0.75).floor() as u32);
        }
        if ch != 1 {
            nh = max1((ch as f64 * 0.75).floor() as u32);
        }
        if nw == cw && nh == ch {
            break;
        }
        cw = nw;
        ch = nh;
    }
    None
}

fn max1(v: u32) -> u32 {
    if v < 1 { 1 } else { v }
}

/// Mirrors pi's discriminated `ProcessImageResult` (utils/image-process.ts).
pub(crate) struct ProcessImageResult {
    pub ok: bool,
    pub data: Vec<u8>,
    pub mime_type: String,
    pub hints: Vec<String>,
    pub message: String,
}

/// Maps the mime types models accept inline. BMP (and any non-listed type) must
/// be converted to PNG before sending. Mirrors pi's
/// `normalizeSupportedImageMimeType`.
fn normalize_supported_image_mime_type(mime_type: &str) -> String {
    let base = base_mime(mime_type);
    match base.as_str() {
        "image/png" => "image/png".to_string(),
        "image/jpeg" | "image/jpg" => "image/jpeg".to_string(),
        "image/gif" => "image/gif".to_string(),
        "image/webp" => "image/webp".to_string(),
        _ => String::new(),
    }
}

fn base_mime(mime_type: &str) -> String {
    let base = mime_type.split(';').next().unwrap_or("").trim();
    base.to_lowercase()
}

/// Decodes arbitrary image bytes and re-encodes as PNG, mirroring pi's
/// `convertImageBytesToPng` (photon). Returns `None` on decode failure (pi
/// returns null). Only BMP reaches this path today.
fn convert_image_bytes_to_png(data: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(data).ok()?;
    let mut buf = Vec::new();
    img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png).ok()?;
    Some(buf)
}

/// Mirrors pi's `conversionHint`: emitted when the image was converted from one
/// mime type to another.
fn conversion_hint(from: &str, to: &str) -> String {
    if from.is_empty() || from == to {
        return String::new();
    }
    format!("[Image converted from {from} to {to}.]")
}

/// A faithful port of pi's `processImage` (utils/image-process.ts). Normalizes
/// the image to a supported inline mime type (converting BMP→PNG), optionally
/// auto-resizes it below the inline limit, and reports processing hints.
pub(crate) fn process_image(data: &[u8], mime_type: &str, auto_resize_images: bool) -> ProcessImageResult {
    let mut normalized_mime = normalize_supported_image_mime_type(mime_type);
    let mut norm_bytes = data.to_vec();
    let mut converted_from = String::new();
    if normalized_mime.is_empty() {
        let Some(png_bytes) = convert_image_bytes_to_png(data) else {
            return ProcessImageResult {
                ok: false,
                data: Vec::new(),
                mime_type: String::new(),
                hints: Vec::new(),
                message: "[Image omitted: could not be converted to a supported inline image format.]"
                    .to_string(),
            };
        };
        norm_bytes = png_bytes;
        normalized_mime = "image/png".to_string();
        converted_from = base_mime(mime_type);
    }

    if auto_resize_images {
        let Some(resized) = resize_image(&norm_bytes, &normalized_mime) else {
            return ProcessImageResult {
                ok: false,
                data: Vec::new(),
                mime_type: String::new(),
                hints: Vec::new(),
                message: "[Image omitted: could not be resized below the inline image size limit.]"
                    .to_string(),
            };
        };
        let mut hints = Vec::new();
        let h = conversion_hint(&converted_from, &resized.mime_type);
        if !h.is_empty() {
            hints.push(h);
        }
        let dn = format_dimension_note(&resized);
        if !dn.is_empty() {
            hints.push(dn);
        }
        return ProcessImageResult {
            ok: true,
            data: resized.data,
            mime_type: resized.mime_type,
            hints,
            message: String::new(),
        };
    }

    let mut hints = Vec::new();
    let h = conversion_hint(&converted_from, &normalized_mime);
    if !h.is_empty() {
        hints.push(h);
    }
    ProcessImageResult {
        ok: true,
        data: norm_bytes,
        mime_type: normalized_mime,
        hints,
        message: String::new(),
    }
}

/// Mirrors pi's `formatDimensionNote`: a coordinate-mapping hint emitted only
/// when the image was resized. The scale uses JS `toFixed(2)` semantics.
fn format_dimension_note(r: &ResizeResult) -> String {
    if !r.was_resized || r.width == 0 {
        return String::new();
    }
    let scale = r.original_width as f64 / r.width as f64;
    format!(
        "[Image: original {}x{}, displayed at {}x{}. Multiply coordinates by {} to map to original image.]",
        r.original_width,
        r.original_height,
        r.width,
        r.height,
        to_fixed2(scale)
    )
}

/// Formats `x` with exactly two decimals, matching JS `Number.toFixed(2)`.
fn to_fixed2(x: f64) -> String {
    format!("{x:.2}")
}

fn encode_under_limit(img: &DynamicImage) -> Option<(Vec<u8>, String)> {
    let mut buf = Vec::new();
    if img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png).is_ok()
        && base64_size(buf.len()) < IMG_MAX_BASE64_BYTES
    {
        return Some((buf, "image/png".to_string()));
    }
    for &q in JPEG_QUALITIES {
        buf = Vec::new();
        if encode_jpeg(img, &mut buf, q).is_ok() && base64_size(buf.len()) < IMG_MAX_BASE64_BYTES {
            return Some((buf, "image/jpeg".to_string()));
        }
    }
    None
}

fn encode_jpeg(img: &DynamicImage, buf: &mut Vec<u8>, quality: u8) -> image::ImageResult<()> {
    use image::codecs::jpeg::JpegEncoder;
    JpegEncoder::new_with_quality(buf, quality).encode_image(img)
}

fn format_to_mime(f: ImageFormat) -> &'static str {
    match f {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        ImageFormat::Bmp => "image/bmp",
        _ => "image/png",
    }
}

// ---------------------------------------------------------------------------
// EXIF orientation (port of pi's `getExifOrientation` + `applyExifOrientation`)
// ---------------------------------------------------------------------------

/// Applies the EXIF orientation found in the original bytes (JPEG or WebP) to
/// `img`, using the `image` crate's geometric ops.
fn apply_exif_orientation_from_bytes(img: &DynamicImage, data: &[u8]) -> DynamicImage {
    let o = exif_orientation_from_bytes(data);
    if o <= 1 {
        return img.clone();
    }
    apply_orientation(img, o)
}

/// Reads the EXIF orientation (1-8) from JPEG or WebP bytes. Returns 1 when
/// absent.
fn exif_orientation_from_bytes(data: &[u8]) -> u32 {
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xD8 {
        return jpeg_orientation(data);
    }
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return webp_orientation(data);
    }
    1
}

/// Extracts the EXIF orientation (1-8) from a JPEG, or 1 if absent.
fn jpeg_orientation(data: &[u8]) -> u32 {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return 1;
    }
    let mut i = 2;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            break;
        }
        let marker = data[i + 1];
        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        let seg_len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if seg_len < 2 || i + 2 + seg_len > data.len() {
            break;
        }
        if marker == 0xE1 {
            let seg = &data[i + 4..i + 2 + seg_len];
            if let Some(o) = exif_orientation(seg) {
                return o;
            }
        }
        i += 2 + seg_len;
    }
    1
}

/// Reads orientation from a WebP EXIF chunk (mirrors pi's `findWebpTiffOffset` +
/// `readOrientationFromTiff`).
fn webp_orientation(data: &[u8]) -> u32 {
    let mut off = 12;
    while off + 8 <= data.len() {
        let chunk_id = &data[off..off + 4];
        let chunk_size = u32::from_le_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]])
            as usize;
        let data_start = off + 8;
        if chunk_id == b"EXIF" {
            if data_start + chunk_size > data.len() {
                return 1;
            }
            let mut tiff = &data[data_start..];
            if chunk_size >= 6 && tiff.len() >= 6 && &tiff[0..6] == b"Exif\x00\x00" {
                tiff = &tiff[6..];
            }
            if let Some(o) = tiff_orientation(tiff) {
                return o;
            }
            return 1;
        }
        off = data_start + chunk_size + (chunk_size % 2);
    }
    1
}

fn exif_orientation(seg: &[u8]) -> Option<u32> {
    if seg.len() < 14 || &seg[0..6] != b"Exif\x00\x00" {
        return None;
    }
    tiff_orientation(&seg[6..])
}

/// Reads the Orientation tag (0x0112) from a TIFF header.
fn tiff_orientation(tiff: &[u8]) -> Option<u32> {
    if tiff.len() < 8 {
        return None;
    }
    let le = &tiff[0..2] == b"II";
    let be = &tiff[0..2] == b"MM";
    if !le && !be {
        return None;
    }
    let rd_u16 = |b: &[u8]| -> u16 {
        if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    };
    let rd_u32 = |b: &[u8]| -> u32 {
        if le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    };
    let ifd_off = rd_u32(&tiff[4..8]) as usize;
    if ifd_off + 2 > tiff.len() {
        return None;
    }
    let count = rd_u16(&tiff[ifd_off..ifd_off + 2]) as usize;
    let mut p = ifd_off + 2;
    for _ in 0..count {
        if p + 12 > tiff.len() {
            break;
        }
        let tag = rd_u16(&tiff[p..p + 2]);
        if tag == 0x0112 {
            let val = rd_u16(&tiff[p + 8..p + 10]) as u32;
            if (1..=8).contains(&val) {
                return Some(val);
            }
            return None;
        }
        p += 12;
    }
    None
}

/// Rotates/flips `img` per the EXIF orientation value (1-8), matching pi's
/// `applyExifOrientation` pixel mapping (via the `image` crate's geometric ops
/// for flips/rotations; transpose/transverse are applied pixel-wise since
/// `image::imageops` does not expose them).
fn apply_orientation(img: &DynamicImage, orientation: u32) -> DynamicImage {
    use image::imageops;
    let wrap = |b: image::ImageBuffer<image::Rgba<u8>, Vec<u8>>| -> DynamicImage {
        DynamicImage::ImageRgba8(b)
    };
    match orientation {
        2 => wrap(imageops::flip_horizontal(img)),
        3 => wrap(imageops::rotate180(img)),
        4 => wrap(imageops::flip_vertical(img)),
        5 => transpose_image(img),
        6 => wrap(imageops::rotate90(img)),
        7 => transverse_image(img),
        8 => wrap(imageops::rotate270(img)),
        _ => img.clone(),
    }
}

/// Transpose (EXIF 5): dst(x, y) = src(y, x). Swaps dimensions.
fn transpose_image(img: &DynamicImage) -> DynamicImage {
    let src = img.to_rgba8();
    let (w, h) = src.dimensions();
    let mut dst = image::ImageBuffer::new(h, w);
    for y in 0..h {
        for x in 0..w {
            dst.put_pixel(y, x, *src.get_pixel(x, y));
        }
    }
    DynamicImage::ImageRgba8(dst)
}

/// Transverse (EXIF 7): dst(x, y) = src(w-1-y, h-1-x). Swaps dimensions.
fn transverse_image(img: &DynamicImage) -> DynamicImage {
    let src = img.to_rgba8();
    let (w, h) = src.dimensions();
    let mut dst = image::ImageBuffer::new(h, w);
    for y in 0..h {
        for x in 0..w {
            let sx = w - 1 - y;
            let sy = h - 1 - x;
            dst.put_pixel(y, x, *src.get_pixel(sx, sy));
        }
    }
    DynamicImage::ImageRgba8(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_size_formula() {
        assert_eq!(base64_size(0), 0);
        assert_eq!(base64_size(1), 4);
        assert_eq!(base64_size(3), 4);
        assert_eq!(base64_size(4), 8);
    }

    #[test]
    fn normalize_supported_mime() {
        assert_eq!(normalize_supported_image_mime_type("image/png"), "image/png");
        assert_eq!(normalize_supported_image_mime_type("image/jpg"), "image/jpeg");
        assert_eq!(normalize_supported_image_mime_type("image/bmp"), "");
        assert_eq!(normalize_supported_image_mime_type("image/png; foo"), "image/png");
    }

    #[test]
    fn resize_small_png_passthrough() {
        // A tiny in-memory PNG stays within limits → returned unchanged.
        let img = image::DynamicImage::new_rgba8(2, 2);
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png).unwrap();
        let r = resize_image(&buf, "image/png").expect("small image should pass through");
        assert!(!r.was_resized);
        assert_eq!(r.data, buf);
        assert_eq!(r.width, 2);
        assert_eq!(r.height, 2);
    }

    #[test]
    fn resize_large_png_uses_expected_bounds() {
        let image = image::DynamicImage::new_rgba8(2401, 1201);
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        let resized = resize_image(&bytes, "image/png").expect("large image should resize");
        assert!(resized.was_resized);
        assert!(resized.width <= IMG_MAX_WIDTH);
        assert!(resized.height <= IMG_MAX_HEIGHT);
        assert_eq!((resized.width, resized.height), (2000, 1000));
        let decoded = image::load_from_memory(&resized.data).unwrap();
        assert_eq!(decoded.dimensions(), (resized.width, resized.height));
    }

    #[test]
    fn process_image_bmp_converts_to_png() {
        // Synthesize a tiny BMP and ensure processImage converts it to PNG.
        let bmp = make_tiny_bmp();
        let r = process_image(&bmp, "image/bmp", false);
        assert!(r.ok);
        assert_eq!(r.mime_type, "image/png");
        assert!(r.hints.iter().any(|h| h.contains("converted from image/bmp")));
    }

    fn make_tiny_bmp() -> Vec<u8> {
        let img = image::DynamicImage::new_rgba8(2, 2);
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Bmp).unwrap();
        buf
    }
}