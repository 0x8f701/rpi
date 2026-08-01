//! Safe terminal image protocol detection, layout, encoding, and frame caching.
//!
//! Raw protocol bytes are emitted only by the interactive TUI's terminal guard.
//! Transcript widgets continue to contain ordinary text and blank cell rows, so
//! JSON, RPC, print mode, logs, and ratatui buffers never receive escape frames.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Cursor, Write};

use base64::Engine as _;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader};
use sha2::{Digest, Sha256};

use crate::image_pipeline;

const DEFAULT_IMAGE_WIDTH_CELLS: u16 = 50;
const DEFAULT_CELL_WIDTH_PIXELS: u16 = 8;
const DEFAULT_CELL_HEIGHT_PIXELS: u16 = 16;
const KITTY_CHUNK_BYTES: usize = 4_096;
const MAX_METADATA_CACHE_ENTRIES: usize = 256;
pub const KITTY_DELETE_ALL: &[u8] = b"\x1b_Ga=d,d=A,q=2\x1b\\";

/// The terminal graphics protocol identified from explicit environment hints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalImageProtocol {
    Kitty,
    Iterm2,
    /// Detected explicitly, but intentionally not emitted without a safe Rust
    /// encoder. The TUI renders the ordinary metadata fallback instead.
    Sixel,
}

/// Terminal environment values used for deterministic capability detection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalEnvironment {
    pub term: Option<String>,
    pub term_program: Option<String>,
    pub kitty_window_id: Option<String>,
    pub iterm_session_id: Option<String>,
    pub tmux: Option<String>,
    pub sty: Option<String>,
}

impl TerminalEnvironment {
    #[must_use]
    pub fn current() -> Self {
        Self {
            term: nonempty_env("TERM"),
            term_program: nonempty_env("TERM_PROGRAM"),
            kitty_window_id: nonempty_env("KITTY_WINDOW_ID"),
            iterm_session_id: nonempty_env("ITERM_SESSION_ID"),
            tmux: nonempty_env("TMUX"),
            sty: nonempty_env("STY"),
        }
    }
}

/// Detect only protocols with explicit, protocol-specific evidence. Multiplexer
/// sessions fall back because their passthrough configuration cannot be proven
/// from the child process environment.
#[must_use]
pub fn detect_protocol(environment: &TerminalEnvironment) -> Option<TerminalImageProtocol> {
    if environment.tmux.is_some() || environment.sty.is_some() {
        return None;
    }

    let term = environment.term.as_deref().unwrap_or_default();
    let term_program = environment.term_program.as_deref().unwrap_or_default();
    if environment.kitty_window_id.is_some()
        || term.eq_ignore_ascii_case("xterm-kitty")
        || term_program.eq_ignore_ascii_case("wezterm")
        || term_program.eq_ignore_ascii_case("ghostty")
    {
        return Some(TerminalImageProtocol::Kitty);
    }
    if term_program.eq_ignore_ascii_case("iterm.app") && environment.iterm_session_id.is_some() {
        return Some(TerminalImageProtocol::Iterm2);
    }
    if term.to_ascii_lowercase().contains("sixel") {
        return Some(TerminalImageProtocol::Sixel);
    }
    None
}

/// Effective settings for one TUI image layout pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageDisplayConfig {
    pub show_images: bool,
    pub width_cells: u16,
}

impl Default for ImageDisplayConfig {
    fn default() -> Self {
        Self {
            show_images: true,
            width_cells: DEFAULT_IMAGE_WIDTH_CELLS,
        }
    }
}
impl ImageDisplayConfig {
    #[must_use]
    pub fn from_terminal_settings(settings: Option<&pi_coding::TerminalSettings>) -> Self {
        Self {
            show_images: settings.and_then(|settings| settings.show_images).unwrap_or(true),
            width_cells: settings
                .and_then(|settings| settings.image_width_cells)
                .unwrap_or(DEFAULT_IMAGE_WIDTH_CELLS)
                .max(1),
        }
    }
}

/// Pixel size of one terminal cell. Zero-valued terminal reports are replaced
/// with a conservative 8x16 cell for deterministic aspect-ratio preservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalCellSize {
    pub width_pixels: u16,
    pub height_pixels: u16,
}

impl Default for TerminalCellSize {
    fn default() -> Self {
        Self {
            width_pixels: DEFAULT_CELL_WIDTH_PIXELS,
            height_pixels: DEFAULT_CELL_HEIGHT_PIXELS,
        }
    }
}

impl TerminalCellSize {
    #[must_use]
    pub fn normalized(self) -> Self {
        let fallback = Self::default();
        Self {
            width_pixels: if self.width_pixels == 0 { fallback.width_pixels } else { self.width_pixels },
            height_pixels: if self.height_pixels == 0 { fallback.height_pixels } else { self.height_pixels },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SourceKey([u8; 32]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct AssetKey {
    source: SourceKey,
    width_pixels: u32,
    height_pixels: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImageMetadata {
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MetadataCacheEntry {
    key: SourceKey,
    metadata: Option<ImageMetadata>,
}

/// Validated image layout. Values can only be constructed by the bounded
/// decoder in [`TerminalImageRenderer::layout`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageLayout {
    asset: AssetKey,
    columns: u16,
    rows: u16,
}

impl ImageLayout {
    #[must_use]
    pub fn columns(self) -> u16 {
        self.columns
    }

    #[must_use]
    pub fn rows(self) -> u16 {
        self.rows
    }
}

/// One fully visible image reservation in the just-drawn ratatui frame.
#[derive(Clone, Debug)]
pub struct ImagePlacement {
    layout: ImageLayout,
    data: String,
    mime_type: String,
    x: u16,
    y: u16,
}

impl ImagePlacement {
    #[must_use]
    pub fn new(layout: ImageLayout, data: impl Into<String>, mime_type: impl Into<String>, x: u16, y: u16) -> Self {
        Self {
            layout,
            data: data.into(),
            mime_type: mime_type.into(),
            x,
            y,
        }
    }
}

/// Identity of the ratatui content underneath image overlays. A changed
/// viewport, theme, or rendered message hash invalidates active overlays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageFrameIdentity {
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub theme_hash: u64,
    pub message_hash: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct RenderIdentity {
    asset: AssetKey,
    columns: u16,
    rows: u16,
    x: u16,
    y: u16,
}

impl From<&ImagePlacement> for RenderIdentity {
    fn from(placement: &ImagePlacement) -> Self {
        Self {
            asset: placement.layout.asset,
            columns: placement.layout.columns,
            rows: placement.layout.rows,
            x: placement.x,
            y: placement.y,
        }
    }
}

#[derive(Clone, Debug)]
struct PreparedImage {
    png_base64: String,
}

/// Stateful overlay renderer. It caches only the currently desired prepared
/// images, preventing transcript scrolling from retransmitting unchanged frames
/// or retaining an unbounded history of decoded image payloads.
pub struct TerminalImageRenderer {
    protocol: Option<TerminalImageProtocol>,
    metadata: VecDeque<MetadataCacheEntry>,
    prepared: HashMap<AssetKey, PreparedImage>,
    active: HashMap<RenderIdentity, u32>,
    frame_identity: Option<ImageFrameIdentity>,
    next_kitty_id: u32,
}

impl Default for TerminalImageRenderer {
    fn default() -> Self {
        Self::new(detect_protocol(&TerminalEnvironment::current()))
    }
}

impl TerminalImageRenderer {
    #[must_use]
    pub fn new(protocol: Option<TerminalImageProtocol>) -> Self {
        Self {
            protocol,
            metadata: VecDeque::new(),
            prepared: HashMap::new(),
            active: HashMap::new(),
            frame_identity: None,
            next_kitty_id: 1,
        }
    }

    #[must_use]
    pub fn protocol(&self) -> Option<TerminalImageProtocol> {
        self.protocol
    }

    /// Sixel is detected for truthful diagnostics, but does not claim rendering
    /// support until the project adopts a bounded safe encoder.
    #[must_use]
    pub fn supports_images(&self) -> bool {
        matches!(
            self.protocol,
            Some(TerminalImageProtocol::Kitty | TerminalImageProtocol::Iterm2)
        )
    }

    /// Decode bounded base64, validate magic/MIME/dimensions through the shared
    /// image pipeline, and compute a viewport-clamped cell reservation.
    /// `None` means the caller must render the ordinary metadata fallback.
    #[must_use]
    pub fn layout(
        &mut self,
        data: &str,
        mime_type: &str,
        config: ImageDisplayConfig,
        viewport_columns: u16,
        viewport_rows: u16,
        cell_size: TerminalCellSize,
    ) -> Option<ImageLayout> {
        if !config.show_images
            || !self.supports_images()
            || viewport_columns == 0
            || viewport_rows == 0
        {
            return None;
        }
        let max_encoded = image_pipeline::MAX_IMAGE_BYTES.div_ceil(3).saturating_mul(4);
        if data.is_empty() || data.len() > max_encoded {
            return None;
        }

        let source = source_key(data, mime_type);
        let metadata = match self.metadata(source) {
            Some(metadata) => metadata,
            None => {
                let metadata = decode_and_validate(data, mime_type).and_then(|bytes| {
                    let format = image::guess_format(&bytes).ok()?;
                    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
                        .into_dimensions()
                        .ok()?;
                    Some(ImageMetadata { width, height })
                });
                self.insert_metadata(source, metadata);
                metadata
            }
        }?;

        let cell_size = cell_size.normalized();
        let mut columns = config.width_cells.max(1).min(viewport_columns);
        let mut rows = cell_rows(metadata, columns, cell_size);
        if rows > viewport_rows {
            columns = columns_for_rows(metadata, viewport_rows, cell_size)
                .max(1)
                .min(columns);
            rows = cell_rows(metadata, columns, cell_size)
                .min(viewport_rows)
                .max(1);
        }

        let requested_width = u32::from(columns).saturating_mul(u32::from(cell_size.width_pixels));
        let width_pixels = requested_width.min(metadata.width).max(1);
        let height_pixels = scaled_height(metadata, width_pixels);
        Some(ImageLayout {
            asset: AssetKey {
                source,
                width_pixels,
                height_pixels,
            },
            columns,
            rows,
        })
    }

    /// Reconcile and emit overlays after ratatui has completed the cell draw.
    /// Repeated calls for the same frame and placements write no bytes.
    pub fn present<W: Write>(
        &mut self,
        writer: &mut W,
        frame: ImageFrameIdentity,
        placements: &[ImagePlacement],
    ) -> io::Result<()> {
        if !self.supports_images() {
            self.active.clear();
            self.prepared.clear();
            self.frame_identity = Some(frame);
            return Ok(());
        }

        if self.frame_identity != Some(frame) {
            self.delete_active_kitty(writer)?;
            self.active.clear();
            self.frame_identity = Some(frame);
        }

        let desired = placements
            .iter()
            .map(RenderIdentity::from)
            .collect::<HashSet<_>>();
        if self.protocol == Some(TerminalImageProtocol::Kitty) {
            let stale = self
                .active
                .iter()
                .filter_map(|(identity, id)| {
                    (!desired.contains(identity)).then_some((*identity, *id))
                })
                .collect::<Vec<_>>();
            for (identity, id) in stale {
                write_kitty_delete(writer, id)?;
                self.active.remove(&identity);
            }
        } else {
            self.active.retain(|identity, _| desired.contains(identity));
        }
        self.prepared
            .retain(|asset, _| desired.iter().any(|identity| identity.asset == *asset));

        for placement in placements {
            let identity = RenderIdentity::from(placement);
            if self.active.contains_key(&identity) {
                continue;
            }
            if source_key(&placement.data, &placement.mime_type) != placement.layout.asset.source {
                continue;
            }
            if !self.prepared.contains_key(&placement.layout.asset) {
                let Some(prepared) = prepare_image(placement) else {
                    continue;
                };
                self.prepared.insert(placement.layout.asset, prepared);
            }
            let allocated_kitty_id = if self.protocol == Some(TerminalImageProtocol::Kitty) {
                Some(self.allocate_kitty_id())
            } else {
                None
            };
            let prepared = self
                .prepared
                .get(&placement.layout.asset)
                .expect("prepared image inserted above");
            write!(
                writer,
                "\x1b7\x1b[{};{}H",
                placement.y.saturating_add(1),
                placement.x.saturating_add(1)
            )?;
            let kitty_id = match self.protocol {
                Some(TerminalImageProtocol::Kitty) => {
                    let id = allocated_kitty_id.expect("Kitty id allocated above");
                    write_kitty_image(writer, id, placement.layout, &prepared.png_base64)?;
                    id
                }
                Some(TerminalImageProtocol::Iterm2) => {
                    write_iterm_image(writer, placement.layout, &prepared.png_base64)?;
                    0
                }
                Some(TerminalImageProtocol::Sixel) | None => 0,
            };
            writer.write_all(b"\x1b8")?;
            self.active.insert(identity, kitty_id);
        }
        writer.flush()
    }

    /// Delete all Kitty image IDs before leaving the alternate screen. Other
    /// protocols have no safe explicit deletion primitive, so their cache is
    /// simply invalidated and ratatui clears the reserved cells.
    pub fn cleanup<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.delete_active_kitty(writer)?;
        self.active.clear();
        self.prepared.clear();
        self.frame_identity = None;
        writer.flush()
    }

    fn metadata(&self, key: SourceKey) -> Option<Option<ImageMetadata>> {
        self.metadata
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.metadata)
    }

    fn insert_metadata(&mut self, key: SourceKey, metadata: Option<ImageMetadata>) {
        if self.metadata.len() == MAX_METADATA_CACHE_ENTRIES {
            self.metadata.pop_front();
        }
        self.metadata
            .push_back(MetadataCacheEntry { key, metadata });
    }

    fn allocate_kitty_id(&mut self) -> u32 {
        let id = self.next_kitty_id.max(1);
        self.next_kitty_id = id.checked_add(1).unwrap_or(1);
        id
    }

    fn delete_active_kitty<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        if self.protocol == Some(TerminalImageProtocol::Kitty) {
            for id in self.active.values().copied().filter(|id| *id != 0) {
                write_kitty_delete(writer, id)?;
            }
        }
        Ok(())
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn source_key(data: &str, mime_type: &str) -> SourceKey {
    let mut digest = Sha256::new();
    digest.update(mime_type.as_bytes());
    digest.update([0]);
    digest.update(data.as_bytes());
    SourceKey(digest.finalize().into())
}

fn decode_and_validate(data: &str, mime_type: &str) -> Option<Vec<u8>> {
    let max_encoded = image_pipeline::MAX_IMAGE_BYTES
        .div_ceil(3)
        .saturating_mul(4);
    if data.is_empty() || data.len() > max_encoded {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .ok()?;
    image_pipeline::validate_image(&bytes, mime_type).ok()?;
    Some(bytes)
}

fn cell_rows(metadata: ImageMetadata, columns: u16, cell_size: TerminalCellSize) -> u16 {
    let numerator = u64::from(metadata.height)
        .saturating_mul(u64::from(columns))
        .saturating_mul(u64::from(cell_size.width_pixels));
    let denominator = u64::from(metadata.width)
        .saturating_mul(u64::from(cell_size.height_pixels))
        .max(1);
    u16::try_from(numerator.div_ceil(denominator))
        .unwrap_or(u16::MAX)
        .max(1)
}

fn columns_for_rows(metadata: ImageMetadata, rows: u16, cell_size: TerminalCellSize) -> u16 {
    let numerator = u64::from(rows)
        .saturating_mul(u64::from(cell_size.height_pixels))
        .saturating_mul(u64::from(metadata.width));
    let denominator = u64::from(metadata.height)
        .saturating_mul(u64::from(cell_size.width_pixels))
        .max(1);
    u16::try_from(numerator / denominator)
        .unwrap_or(u16::MAX)
        .max(1)
}

fn scaled_height(metadata: ImageMetadata, width: u32) -> u32 {
    let numerator = u64::from(metadata.height).saturating_mul(u64::from(width));
    let height = numerator.div_ceil(u64::from(metadata.width).max(1));
    u32::try_from(height).unwrap_or(u32::MAX).max(1)
}

fn prepare_image(placement: &ImagePlacement) -> Option<PreparedImage> {
    let bytes = decode_and_validate(&placement.data, &placement.mime_type)?;
    let format = image::guess_format(&bytes).ok()?;
    let decoded = ImageReader::with_format(Cursor::new(bytes), format)
        .decode()
        .ok()?;
    let image = resize_for_layout(decoded, placement.layout.asset);
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    if png.len() > image_pipeline::MAX_IMAGE_BYTES {
        return None;
    }
    Some(PreparedImage {
        png_base64: base64::engine::general_purpose::STANDARD.encode(png),
    })
}

fn resize_for_layout(image: DynamicImage, asset: AssetKey) -> DynamicImage {
    if image.width() == asset.width_pixels && image.height() == asset.height_pixels {
        image
    } else {
        image.resize_exact(
            asset.width_pixels,
            asset.height_pixels,
            FilterType::Lanczos3,
        )
    }
}

fn write_kitty_image<W: Write>(
    writer: &mut W,
    id: u32,
    layout: ImageLayout,
    payload: &str,
) -> io::Result<()> {
    let mut chunks = payload.as_bytes().chunks(KITTY_CHUNK_BYTES).peekable();
    let Some(first) = chunks.next() else {
        return Ok(());
    };
    let more = u8::from(chunks.peek().is_some());
    write!(
        writer,
        "\x1b_Ga=T,f=100,t=d,i={id},q=2,c={},r={},m={more};",
        layout.columns, layout.rows
    )?;
    writer.write_all(first)?;
    writer.write_all(b"\x1b\\")?;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        write!(writer, "\x1b_Gm={more};")?;
        writer.write_all(chunk)?;
        writer.write_all(b"\x1b\\")?;
    }
    Ok(())
}

fn write_kitty_delete<W: Write>(writer: &mut W, id: u32) -> io::Result<()> {
    write!(writer, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

fn write_iterm_image<W: Write>(
    writer: &mut W,
    layout: ImageLayout,
    payload: &str,
) -> io::Result<()> {
    write!(
        writer,
        "\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio=1:{}\x07",
        layout.columns, layout.rows, payload
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    fn png(width: u32, height: u32) -> String {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_fn(width, height, |x, y| {
            Rgba([
                (x.wrapping_mul(31).wrapping_add(y)) as u8,
                (y.wrapping_mul(17).wrapping_add(x)) as u8,
                x.wrapping_mul(y).wrapping_add(19) as u8,
                255,
            ])
        }));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn frame(message_hash: u64) -> ImageFrameIdentity {
        ImageFrameIdentity {
            viewport_width: 80,
            viewport_height: 24,
            theme_hash: 1,
            message_hash,
        }
    }

    #[test]
    fn terminal_settings_apply_defaults_and_overrides() {
        assert_eq!(ImageDisplayConfig::from_terminal_settings(None), ImageDisplayConfig::default());
        let settings = pi_coding::TerminalSettings {
            show_images: Some(false),
            image_width_cells: Some(0),
            ..pi_coding::TerminalSettings::default()
        };
        assert_eq!(
            ImageDisplayConfig::from_terminal_settings(Some(&settings)),
            ImageDisplayConfig { show_images: false, width_cells: 1 }
        );
    }
    #[test]
    fn kitty_delete_all_frame_is_bounded_and_quiet() {
        assert_eq!(KITTY_DELETE_ALL, b"\x1b_Ga=d,d=A,q=2\x1b\\");
    }


    #[test]
    fn detection_requires_explicit_protocol_evidence_and_rejects_multiplexers() {
        let kitty = TerminalEnvironment {
            kitty_window_id: Some("1".to_owned()),
            ..TerminalEnvironment::default()
        };
        assert_eq!(detect_protocol(&kitty), Some(TerminalImageProtocol::Kitty));
        let iterm = TerminalEnvironment {
            term_program: Some("iTerm.app".to_owned()),
            iterm_session_id: Some("w0t0p0".to_owned()),
            ..TerminalEnvironment::default()
        };
        assert_eq!(detect_protocol(&iterm), Some(TerminalImageProtocol::Iterm2));
        let sixel = TerminalEnvironment {
            term: Some("xterm-sixel".to_owned()),
            ..TerminalEnvironment::default()
        };
        assert_eq!(detect_protocol(&sixel), Some(TerminalImageProtocol::Sixel));
        assert!(!TerminalImageRenderer::new(detect_protocol(&sixel)).supports_images());
        let tmux = TerminalEnvironment {
            tmux: Some("/tmp/tmux".to_owned()),
            ..kitty
        };
        assert_eq!(detect_protocol(&tmux), None);
        assert_eq!(detect_protocol(&TerminalEnvironment::default()), None);
    }

    #[test]
    fn layout_is_bounded_validated_and_aspect_preserving() {
        let mut renderer = TerminalImageRenderer::new(Some(TerminalImageProtocol::Kitty));
        let data = png(400, 200);
        let layout = renderer
            .layout(
                &data,
                "image/png",
                ImageDisplayConfig {
                    show_images: true,
                    width_cells: 100,
                },
                30,
                20,
                TerminalCellSize {
                    width_pixels: 8,
                    height_pixels: 16,
                },
            )
            .unwrap();
        assert_eq!(layout.columns(), 30);
        assert_eq!(layout.rows(), 8);
        assert!(
            renderer
                .layout(
                    &data,
                    "image/jpeg",
                    ImageDisplayConfig::default(),
                    80,
                    24,
                    TerminalCellSize::default(),
                )
                .is_none()
        );
        assert!(
            renderer
                .layout(
                    &"A".repeat(image_pipeline::MAX_IMAGE_BYTES.div_ceil(3) * 4 + 1),
                    "image/png",
                    ImageDisplayConfig::default(),
                    80,
                    24,
                    TerminalCellSize::default(),
                )
                .is_none()
        );
    }

    #[test]
    fn show_false_and_unsupported_protocol_always_fall_back() {
        let data = png(2, 2);
        let mut kitty = TerminalImageRenderer::new(Some(TerminalImageProtocol::Kitty));
        assert!(
            kitty
                .layout(
                    &data,
                    "image/png",
                    ImageDisplayConfig {
                        show_images: false,
                        width_cells: 50
                    },
                    80,
                    24,
                    TerminalCellSize::default(),
                )
                .is_none()
        );
        let mut sixel = TerminalImageRenderer::new(Some(TerminalImageProtocol::Sixel));
        assert!(
            sixel
                .layout(
                    &data,
                    "image/png",
                    ImageDisplayConfig::default(),
                    80,
                    24,
                    TerminalCellSize::default(),
                )
                .is_none()
        );
    }

    #[test]
    fn kitty_protocol_is_chunked_cached_invalidated_and_cleaned_up() {
        let data = png(160, 160);
        let mut renderer = TerminalImageRenderer::new(Some(TerminalImageProtocol::Kitty));
        let layout = renderer
            .layout(
                &data,
                "image/png",
                ImageDisplayConfig {
                    show_images: true,
                    width_cells: 20,
                },
                80,
                24,
                TerminalCellSize::default(),
            )
            .unwrap();
        let placement = ImagePlacement::new(layout, &data, "image/png", 3, 4);
        let mut first = Vec::new();
        renderer
            .present(&mut first, frame(1), std::slice::from_ref(&placement))
            .unwrap();
        let first = String::from_utf8(first).unwrap();
        assert!(first.starts_with("\u{1b}7\u{1b}[5;4H\u{1b}_Ga=T,f=100,t=d,i=1"));
        assert!(
            first.contains("\u{1b}_Gm="),
            "payload should require continuation chunks"
        );
        for protocol_frame in first.split("\u{1b}\\") {
            // Each Kitty chunk is `\x1b_G<control>;<payload>`. The leading
            // cursor-positioning CSI on the first chunk (`\x1b[<row>;<col>H`)
            // also contains a `;`, so isolate the payload by splitting after
            // the `\x1b_G` APC introducer rather than on the first `;` in the
            // segment, which would wrongly fold the cursor suffix into the
            // payload and exceed KITTY_CHUNK_BYTES.
            let Some(after_apc) = protocol_frame
                .split_once("\u{1b}_G")
                .map(|(_, rest)| rest)
            else {
                continue;
            };
            if let Some((_, payload)) = after_apc.split_once(';') {
                assert!(
                    payload.len() <= KITTY_CHUNK_BYTES,
                    "kitty chunk payload {} exceeds {KITTY_CHUNK_BYTES}",
                    payload.len()
                );
            }
        }
        assert!(first.ends_with("\u{1b}8"));

        let mut cached = Vec::new();
        renderer
            .present(&mut cached, frame(1), std::slice::from_ref(&placement))
            .unwrap();
        assert!(cached.is_empty(), "unchanged frames must not retransmit");

        let mut changed = Vec::new();
        renderer
            .present(&mut changed, frame(2), std::slice::from_ref(&placement))
            .unwrap();
        let changed = String::from_utf8(changed).unwrap();
        assert!(changed.contains("a=d,d=I,i=1"));
        assert!(changed.contains("a=T,f=100,t=d,i=2"));

        let mut cleanup = Vec::new();
        renderer.cleanup(&mut cleanup).unwrap();
        assert!(String::from_utf8(cleanup).unwrap().contains("a=d,d=I,i=2"));
    }

    #[test]
    fn iterm_protocol_bytes_are_isolated_and_cached() {
        let data = png(8, 4);
        let mut renderer = TerminalImageRenderer::new(Some(TerminalImageProtocol::Iterm2));
        let layout = renderer
            .layout(
                &data,
                "image/png",
                ImageDisplayConfig {
                    show_images: true,
                    width_cells: 4,
                },
                80,
                24,
                TerminalCellSize::default(),
            )
            .unwrap();
        let placement = ImagePlacement::new(layout, &data, "image/png", 0, 0);
        let mut output = Vec::new();
        renderer
            .present(&mut output, frame(1), std::slice::from_ref(&placement))
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("\u{1b}7\u{1b}[1;1H\u{1b}]1337;File=inline=1"));
        assert!(output.contains("preserveAspectRatio=1:"));
        assert!(output.ends_with("\u{7}\u{1b}8"));
        let mut cached = Vec::new();
        renderer
            .present(&mut cached, frame(1), std::slice::from_ref(&placement))
            .unwrap();
        assert!(cached.is_empty());
    }
}
