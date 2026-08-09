//! `inspect_image` tool: bounded, deterministic metadata + statistics for an
//! image file, so the model can reason about an image without rendering it.
//!
//! Reports format, dimensions, decoded color type, file size, EXIF orientation
//! (JPEG/WebP only — read cheaply from the raw bytes), mean/stddev 8-bit luma
//! brightness, and a coarse 8-bin RGB dominant-color histogram. Output is a
//! plain-text report structurally bounded to a few KiB. No OCR, no ML.
//!
//! Calibration provenance (memory://src/coding-agents): OMP ships a built-in
//! `inspect_image` tool (`pi-xyz/oh-my-pi/subsystem-tools.md:344-352`) that
//! loads a local image through `loadImageInput()` (dimension gating + optional
//! scaling) and sends it to a vision-capable model. This port deliberately
//! diverges: it performs the inspection locally and deterministically instead
//! of calling a vision model, per the wave contract ("NO OCR, NO ML; give the
//! model enough to reason about an image without rendering it"). It keeps
//! OMP's tool name, the required `path` argument, and the load-time gating
//! intent — a 32 MiB file-size gate before any read, plus a pixel budget for
//! the statistics pass (deterministic thumbnail for very large images). The
//! The vision-prompt `question` argument is intentionally omitted; image
//! *generation* lives in the sibling `generate_image` tool
//! (`tools/image_gen.rs`) with its own provider subsystem (OpenAI-compatible
//! `images/generations`). The rpi survey
//! roadmap (surveys/cli-coding-agents-architecture-comparison.md:898-904)
//! calls for "bounded file/network input, MIME/type validation, artifact
//! limits, and deterministic error classification" — all four are implemented
//! here.
//!
//! The module is named `image` per the wave's file plan; `use ::image as image`
//! below aliases the image crate so this module's name cannot shadow it.

use std::io::Cursor;

use anyhow::{anyhow, Result};
use serde_json::Value;

use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCallContext, ToolCapability};

use crate::tools::imageresize::exif_orientation_from_bytes;
use crate::tools::paths::resolve_scoped_path;
use crate::truncate::format_size;
use crate::WorkspaceRoots;

use ::image as image;
use image::{GenericImageView, ImageFormat};

/// `inspect_image` refuses files larger than this. Enforced from metadata
/// BEFORE any read, so an oversized (even sparse) file is rejected without
/// touching its contents.
const MAX_INSPECT_BYTES: u64 = 32 * 1024 * 1024; // 32 MiB

/// Decoded-memory safety limit, mirroring the tree's image pipeline
/// (`crates/pi-cli/src/image_pipeline.rs:10-12`): a file may be small on disk
/// yet decompress to a huge bitmap, so the decoded allocation is capped at
/// 128 MiB and images above `MAX_IMAGE_PIXELS` are rejected up front.
const MAX_DECODE_ALLOC_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DECODED_BYTES_PER_PIXEL: u64 = 8;
const MAX_IMAGE_PIXELS: u64 = MAX_DECODE_ALLOC_BYTES / MAX_DECODED_BYTES_PER_PIXEL;

/// Pixel budget for the statistics pass. Larger images are deterministically
/// downsampled first so the pass stays cheap and its output bounded.
const MAX_STATS_PIXELS: u64 = 8_000_000;

/// At most this many dominant-color buckets are reported (bounded output).
const MAX_DOMINANT_COLOR_BUCKETS: usize = 3;

/// Coarse 8-bin RGB cube: each channel thresholded at 128 selects one bit
/// (r=4, g=2, b=1). Index → corner color name.
const COLOR_BUCKET_NAMES: [&str; 8] = [
    "black",   // 000
    "blue",    // 001
    "green",   // 010
    "cyan",    // 011
    "red",     // 100
    "magenta", // 101
    "yellow",  // 110
    "white",   // 111
];

/// Builds the `inspect_image` tool rooted at `cwd` (workspace-contained).
pub(crate) fn inspect_image_tool(cwd: &str) -> AgentTool {
    let workspace = super::factory_workspace(cwd);
    let params = super::s_object(
        vec![(
            "path",
            super::s_string(
                "Path to the image file to inspect (relative or absolute; must resolve inside the workspace)",
            ),
        )],
        vec!["path"],
    );
    let description = "Inspect an image file without rendering it: format, dimensions, color type, \
        file size, EXIF orientation (JPEG/WebP), mean/stddev brightness, and a coarse \
        dominant-color histogram. Deterministic, bounded (<=4 KiB) text output (no vision model \
        call; statistics for very large images use a deterministic downsampled view). Supports \
        PNG, JPEG, GIF, BMP, and WebP up to 32 MiB and 16 megapixels. No OCR, no ML. For image \
        generation, use the generate_image tool."
        .to_string();
    AgentTool::new("inspect_image", description, params, move |ctx| {
        let workspace = workspace.clone();
        async move { run_inspect_image(&workspace, ctx.arguments, ctx.abort).await }
    })
    .with_capability(ToolCapability::Read)
    .with_prompt_guidelines(vec![
        "Use inspect_image to reason about an image's metadata and statistics without rendering it."
            .to_string(),
    ])
}

/// Action-less single-purpose execution: reads `args.path`, validates it, and
/// renders the bounded report (or an actionable error).
async fn run_inspect_image(
    workspace: &WorkspaceRoots,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    super::check_aborted(&abort)?;
    let path = super::arg_str(&args, "path");
    if path.is_empty() {
        return Err(anyhow!("File path must not be empty."));
    }
    // Workspace containment via the same helper the other scoped tools use:
    // relative paths resolve from the primary working directory, and symlinks
    // cannot escape the boundary.
    let abs = resolve_scoped_path(&path, workspace)?;
    super::check_aborted(&abort)?;

    // Size bound is enforced from metadata BEFORE any read: an oversized file
    // (including a sparse one) is rejected without touching its contents.
    let info = std::fs::metadata(&abs)
        .map_err(|e| anyhow!("Could not stat image {}: {}", abs, e))?;
    super::check_aborted(&abort)?;
    if info.is_dir() {
        return Err(anyhow!("EISDIR: illegal operation on a directory, inspect_image"));
    }
    let len = info.len();
    if len > MAX_INSPECT_BYTES {
        return Err(anyhow!(
            "File {} is {} ({} bytes), exceeding the inspect_image limit of 32 MiB. \
             Downscale or split the image before inspecting it.",
            abs,
            format_size(len as usize),
            len
        ));
    }

    let data = std::fs::read(&abs).map_err(|e| anyhow!("Could not read image {}: {}", abs, e))?;
    super::check_aborted(&abort)?;

    // Format is guessed from the magic bytes; decode errors distinguish
    // unsupported formats from corrupt/truncated files.
    let mut reader = image::ImageReader::new(Cursor::new(&data));
    reader = reader
        .with_guessed_format()
        .map_err(|e| anyhow!("Could not determine the image format of {}: {}", abs, e))?;
    let format = reader
        .format()
        .ok_or_else(|| unsupported_format_error(&abs))?;
    // Decoded-memory pre-check (mirrors image_pipeline.rs image_dimensions):
    // reject decompression bombs from the header before allocating anything.
    let (w, h) = image::ImageReader::with_format(Cursor::new(&data), format)
        .into_dimensions()
        .map_err(|e| {
            anyhow!(
                "Could not decode image {}: {} (the file may be corrupt or truncated)",
                abs,
                e
            )
        })?;
    if u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
        return Err(anyhow!(
            "Image {} dimensions {}x{} exceed the {} MiB decoded-memory safety limit; \
             downscale the image before inspecting it.",
            abs,
            w,
            h,
            MAX_DECODE_ALLOC_BYTES / 1024 / 1024
        ));
    }
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| match e {
        image::ImageError::Unsupported(_) => unsupported_format_error(&abs),
        image::ImageError::Limits(_) => anyhow!(
            "Image {} exceeds the {} MiB decoded-memory safety limit; downscale the image \
             before inspecting it.",
            abs,
            MAX_DECODE_ALLOC_BYTES / 1024 / 1024
        ),
        other => anyhow!(
            "Could not decode image {}: {} (the file may be corrupt or truncated)",
            abs,
            other
        ),
    })?;
    super::check_aborted(&abort)?;

    let stats = image_stats(&img);
    // EXIF orientation is cheaply available for JPEG/WebP only (read from the
    // raw bytes by the shared helper); other formats carry no EXIF.
    let orientation = matches!(format, ImageFormat::Jpeg | ImageFormat::WebP)
        .then(|| exif_orientation_from_bytes(&data));

    let mut out = String::new();
    out.push_str(&format!("Format: {}\n", format_name(format)));
    out.push_str(&format!(
        "Dimensions: {}x{} ({})\n",
        w,
        h,
        aspect_label(w, h)
    ));
    out.push_str(&format!("Color type: {:?}\n", img.color()));
    out.push_str(&format!(
        "File size: {} ({} bytes)\n",
        format_size(len as usize),
        len
    ));
    if let Some(o) = orientation {
        out.push_str(&format!("EXIF orientation: {} ({})\n", o, orientation_name(o)));
    }
    out.push_str(&format!(
        "Brightness (8-bit luma): mean {:.1}, stddev {:.1} (of 255)\n",
        stats.mean, stats.stddev
    ));
    out.push_str("Dominant colors (coarse 8-bin RGB histogram):\n");
    for (name, pct) in stats.dominant_colors {
        out.push_str(&format!("  {name}: {pct:.1}%\n"));
    }
    Ok(super::text_result(out))
}

fn unsupported_format_error(path: &str) -> anyhow::Error {
    anyhow!("Unsupported image format: {path} (expected PNG, JPEG, GIF, BMP, or WebP)")
}

/// Stable, human-readable format names for the formats `inspect_image` decodes
/// (the workspace image crate is built with exactly these five features).
fn format_name(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Gif => "GIF",
        ImageFormat::Bmp => "BMP",
        ImageFormat::WebP => "WebP",
        _ => "Unknown",
    }
}

fn aspect_label(w: u32, h: u32) -> &'static str {
    match w.cmp(&h) {
        std::cmp::Ordering::Greater => "landscape",
        std::cmp::Ordering::Less => "portrait",
        std::cmp::Ordering::Equal => "square",
    }
}

/// Human names for the EXIF orientation values 1-8 (JPEG/WebP).
fn orientation_name(orientation: u32) -> &'static str {
    match orientation {
        1 => "normal",
        2 => "flipped horizontally",
        3 => "rotated 180",
        4 => "flipped vertically",
        5 => "transposed",
        6 => "rotated 90 CW",
        7 => "transversed",
        8 => "rotated 270 CW",
        _ => "unknown",
    }
}

struct ImageStats {
    mean: f64,
    stddev: f64,
    dominant_colors: Vec<(&'static str, f64)>,
}

/// Computes bounded, deterministic statistics: mean/population-stddev
/// brightness over 8-bit luma and a coarse 8-bin RGB dominant-color histogram.
/// Images above the pixel budget are deterministically downsampled first so
/// the pass stays cheap and the report bounded.
fn image_stats(img: &image::DynamicImage) -> ImageStats {
    let (w, h) = img.dimensions();
    let total = u64::from(w) * u64::from(h);
    let stats_img: image::DynamicImage = if total > MAX_STATS_PIXELS {
        let scale = (MAX_STATS_PIXELS as f64 / total as f64).sqrt();
        let nw = ((f64::from(w) * scale).floor() as u32).max(1);
        let nh = ((f64::from(h) * scale).floor() as u32).max(1);
        img.thumbnail(nw, nh)
    } else {
        img.clone()
    };

    let luma = stats_img.to_luma8();
    let n = u64::from(luma.width()) * u64::from(luma.height());
    if n == 0 {
        return ImageStats {
            mean: 0.0,
            stddev: 0.0,
            dominant_colors: Vec::new(),
        };
    }
    let mut sum: u64 = 0;
    let mut sum_sq: u64 = 0;
    for pixel in luma.pixels() {
        let v = u64::from(pixel.0[0]);
        sum += v;
        sum_sq += v * v;
    }
    let nf = n as f64;
    let mean = sum as f64 / nf;
    let variance = (sum_sq as f64 / nf) - mean * mean;
    ImageStats {
        mean,
        stddev: variance.max(0.0).sqrt(),
        dominant_colors: dominant_color_buckets(&stats_img),
    }
}

/// Coarse 8-bin histogram over the RGB cube (each channel thresholded at 128).
/// Returns the top [`MAX_DOMINANT_COLOR_BUCKETS`] non-empty buckets as
/// (color name, percentage), ordered by share with ties broken by bucket
/// index — fully deterministic.
fn dominant_color_buckets(img: &image::DynamicImage) -> Vec<(&'static str, f64)> {
    let rgb = img.to_rgb8();
    let mut counts = [0u64; 8];
    for pixel in rgb.pixels() {
        let index = ((usize::from(pixel.0[0]) >> 7) << 2)
            | ((usize::from(pixel.0[1]) >> 7) << 1)
            | (usize::from(pixel.0[2]) >> 7);
        counts[index] += 1;
    }
    // `ImageBuffer::len()` counts subpixels (3x for Rgb8), so the total is
    // derived from the dimensions instead.
    let total = u64::from(rgb.width()) * u64::from(rgb.height());
    if total == 0 {
        return Vec::new();
    }
    let mut buckets: Vec<(usize, u64)> = counts
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(index, count)| (index, *count))
        .collect();
    buckets.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    buckets.truncate(MAX_DOMINANT_COLOR_BUCKETS);
    let total_f = total as f64;
    buckets
        .into_iter()
        .map(|(index, count)| (COLOR_BUCKET_NAMES[index], count as f64 / total_f * 100.0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::ContentBlock;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pi-image-inspect-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_ctx(args: Value) -> ToolCallContext {
        let (_ctrl, abort) = pi_agent::AbortController::new();
        std::mem::forget(_ctrl);
        ToolCallContext {
            tool_call_id: "test".to_string(),
            arguments: args,
            on_update: Arc::new(|_r: AgentToolResult| {}),
            abort,
            model: None,
        }
    }

    fn text_of(res: &AgentToolResult) -> String {
        match res.content.first() {
            Some(ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    /// A 4x1 PNG: two black pixels, one red, one white. Deterministic and
    /// unambiguous for the coarse 8-bin histogram (black 50%, red 25%,
    /// white 25%).
    fn fixture_png_bytes() -> Vec<u8> {
        let img = image::RgbaImage::from_fn(4, 1, |x, _| match x {
            0 | 1 => image::Rgba([0, 0, 0, 255]),
            2 => image::Rgba([255, 0, 0, 255]),
            _ => image::Rgba([255, 255, 255, 255]),
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    /// A hand-crafted 24-bit BMP header claiming 16384x16384 (54 bytes total,
    /// no pixel data): parses cleanly but would decode to ~805 MB.
    fn huge_dimension_bmp_bytes() -> Vec<u8> {
        let mut b = Vec::with_capacity(54);
        b.extend_from_slice(b"BM");
        b.extend_from_slice(&54u32.to_le_bytes()); // declared file size (ignored by the header parse)
        b.extend_from_slice(&0u32.to_le_bytes()); // reserved
        b.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
        b.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
        b.extend_from_slice(&16_384i32.to_le_bytes()); // width
        b.extend_from_slice(&16_384i32.to_le_bytes()); // height
        b.extend_from_slice(&1u16.to_le_bytes()); // planes
        b.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
        b.extend_from_slice(&0u32.to_le_bytes()); // compression (BI_RGB)
        b.extend_from_slice(&0u32.to_le_bytes()); // image size
        b.extend_from_slice(&0u32.to_le_bytes()); // horizontal resolution
        b.extend_from_slice(&0u32.to_le_bytes()); // vertical resolution
        b.extend_from_slice(&0u32.to_le_bytes()); // colors used
        b.extend_from_slice(&0u32.to_le_bytes()); // colors important
        b
    }

    #[tokio::test]
    async fn png_report_is_deterministic_and_correct() {
        let d = tmpdir();
        fs::write(d.join("four.png"), fixture_png_bytes()).unwrap();
        let tool = inspect_image_tool(&d.to_string_lossy());

        let res = (tool.execute)(make_ctx(json!({ "path": "four.png" })))
            .await
            .unwrap();
        let text = text_of(&res);
        assert!(text.contains("Format: PNG"), "{text}");
        assert!(text.contains("Dimensions: 4x1 (landscape)"), "{text}");
        assert!(text.contains("Color type: Rgba8"), "{text}");
        assert!(text.contains("File size:"), "{text}");
        assert!(
            text.contains("Brightness (8-bit luma): mean") && text.contains("stddev"),
            "{text}"
        );
        assert!(
            text.contains("Dominant colors (coarse 8-bin RGB histogram):"),
            "{text}"
        );
        assert!(text.contains("  black: 50.0%"), "{text}");
        assert!(text.contains("  red: 25.0%"), "{text}");
        assert!(text.contains("  white: 25.0%"), "{text}");
        // PNG carries no EXIF → no orientation line.
        assert!(!text.contains("EXIF orientation"), "{text}");
        // Report is bounded to a few KiB.
        assert!(text.len() <= 4 * 1024, "report too large: {} bytes", text.len());

        // Deterministic: a second run yields byte-identical output.
        let again = (tool.execute)(make_ctx(json!({ "path": "four.png" })))
            .await
            .unwrap();
        assert_eq!(text_of(&again), text);
    }

    #[tokio::test]
    async fn jpeg_reports_format_and_exif_orientation() {
        let d = tmpdir();
        let img = image::DynamicImage::new_rgba8(16, 16);
        let mut bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Jpeg)
            .unwrap();
        fs::write(d.join("photo.jpg"), bytes).unwrap();
        let tool = inspect_image_tool(&d.to_string_lossy());

        let res = (tool.execute)(make_ctx(json!({ "path": "photo.jpg" })))
            .await
            .unwrap();
        let text = text_of(&res);
        assert!(text.contains("Format: JPEG"), "{text}");
        assert!(text.contains("Dimensions: 16x16 (square)"), "{text}");
        assert!(text.contains("EXIF orientation: 1 (normal)"), "{text}");
    }

    #[tokio::test]
    async fn missing_file_reports_actionable_error() {
        let d = tmpdir();
        let tool = inspect_image_tool(&d.to_string_lossy());
        let err = (tool.execute)(make_ctx(json!({ "path": "nope.png" })))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No such file or directory"), "{msg}");
        assert!(msg.contains("nope.png"), "{msg}");
    }

    #[tokio::test]
    async fn oversized_file_is_rejected_from_metadata_without_reading() {
        let d = tmpdir();
        let path = d.join("huge.png");
        fs::write(&path, b"x").unwrap();
        // Sparse-extend past the limit: the file is never read, only stat'ed.
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(MAX_INSPECT_BYTES + 1).unwrap();
        drop(f);
        let tool = inspect_image_tool(&d.to_string_lossy());
        let err = (tool.execute)(make_ctx(json!({ "path": "huge.png" })))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exceeding the inspect_image limit of 32 MiB"), "{msg}");
        assert!(msg.contains("33554433 bytes"), "{msg}");
    }

    #[tokio::test]
    async fn corrupt_file_reports_decode_error() {
        let d = tmpdir();
        // Valid PNG magic bytes, garbage payload: the format is guessed but
        // decoding must fail.
        fs::write(
            d.join("broken.png"),
            [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
        )
        .unwrap();
        let tool = inspect_image_tool(&d.to_string_lossy());
        let err = (tool.execute)(make_ctx(json!({ "path": "broken.png" })))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Could not decode image"), "{msg}");
        assert!(msg.contains("corrupt or truncated"), "{msg}");
    }

    #[tokio::test]
    async fn unsupported_format_reports_actionable_error() {
        let d = tmpdir();
        fs::write(d.join("data.bin"), b"not an image at all").unwrap();
        let tool = inspect_image_tool(&d.to_string_lossy());
        let err = (tool.execute)(make_ctx(json!({ "path": "data.bin" })))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Unsupported image format"), "{msg}");
        assert!(msg.contains("PNG, JPEG, GIF, BMP, or WebP"), "{msg}");
    }

    #[tokio::test]
    async fn decompression_bomb_is_rejected_from_header_before_decode() {
        let d = tmpdir();
        // A 24-bit BMP whose header claims 16384x16384: tiny on disk (54 bytes,
        // no pixel data) but decoding it would allocate ~805 MB. The
        // decoded-memory pre-check must reject it from the header alone.
        fs::write(d.join("bomb.bmp"), huge_dimension_bmp_bytes()).unwrap();
        let tool = inspect_image_tool(&d.to_string_lossy());
        let err = (tool.execute)(make_ctx(json!({ "path": "bomb.bmp" })))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("decoded-memory safety limit"), "{msg}");
        assert!(msg.contains("16384x16384"), "{msg}");
    }

    #[tokio::test]
    async fn path_outside_workspace_is_rejected() {
        let outside = tmpdir();
        fs::write(outside.join("secret.png"), fixture_png_bytes()).unwrap();
        let cwd = tmpdir();
        let tool = inspect_image_tool(&cwd.to_string_lossy());
        let abs = outside.join("secret.png");
        let err = (tool.execute)(make_ctx(json!({ "path": abs })))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("escapes working directory"), "{err}");
    }

    #[tokio::test]
    async fn empty_path_is_rejected() {
        let d = tmpdir();
        let tool = inspect_image_tool(&d.to_string_lossy());
        let err = (tool.execute)(make_ctx(json!({ "path": "" })))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "File path must not be empty.");
    }

    #[test]
    fn large_image_stats_are_downsampled_to_the_pixel_budget() {
        // 3000x3000 = 9M pixels > 8M budget → downsampled, deterministic.
        let img = image::DynamicImage::new_rgba8(3000, 3000);
        let a = image_stats(&img);
        let b = image_stats(&img);
        assert!(a.mean.abs() < f64::EPSILON, "mean = {}", a.mean);
        assert_eq!(a.stddev, b.stddev);
        assert_eq!(a.dominant_colors, b.dominant_colors);
        assert_eq!(a.dominant_colors.len(), 1);
        assert_eq!(a.dominant_colors[0].0, "black");
        assert_eq!(a.dominant_colors[0].1, 100.0);
    }
}
