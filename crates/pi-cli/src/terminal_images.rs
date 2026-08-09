//! Safe terminal image protocol detection, layout, encoding, and frame caching.
//!
//! Raw protocol bytes are emitted only by the interactive TUI's terminal guard.
//! Transcript widgets continue to contain ordinary text and blank cell rows, so
//! JSON, RPC, print mode, logs, and ratatui buffers never receive escape frames.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Cursor, Read, Write};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use base64::Engine as _;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader};
use sha2::{Digest, Sha256};

use crate::image_pipeline;

const DEFAULT_IMAGE_WIDTH_CELLS: u16 = 60;
const DEFAULT_CELL_WIDTH_PIXELS: u16 = 8;
const DEFAULT_CELL_HEIGHT_PIXELS: u16 = 16;
const KITTY_CHUNK_BYTES: usize = 4_096;
const MAX_METADATA_CACHE_ENTRIES: usize = 256;
const KITTY_QUERY_ID: u32 = 31;
const KITTY_SUPPORT_QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";

/// tmux DCS passthrough prefix. tmux strips `\x1bPtmux;` plus the trailing
/// `\x1b\\` and forwards the wrapped escape verbatim to the outer terminal
/// (requires `allow-passthrough`, which defaults to on since tmux 3.0a).
const TMUX_PASSTHROUGH_PREFIX: &[u8] = b"\x1bPtmux;";
const TMUX_PASSTHROUGH_SUFFIX: &[u8] = b"\x1b\\";
/// First tmux release that reliably forwards wrapped kitty graphics. Older
/// tmux drops the APC even when wrapped, so sessions at those versions keep
/// the previous no-protocol fallback.
const TMUX_KITTY_SUPPORT: (u32, u32) = (3, 3);
/// Bounded budget for the one-shot `tmux -V` probe and its poll interval.
const TMUX_VERSION_TIMEOUT: Duration = Duration::from_millis(2_000);
const TMUX_VERSION_POLL: Duration = Duration::from_millis(10);

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
/// [`TerminalEnvironment::current`] snapshots the process environment; the
/// tmux version probe is bounded and runs at most once per process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalEnvironment {
    pub term: Option<String>,
    pub term_program: Option<String>,
    pub kitty_window_id: Option<String>,
    pub iterm_session_id: Option<String>,
    pub tmux: Option<String>,
    pub sty: Option<String>,
    /// Raw `tmux -V` stdout ("tmux 3.4"), populated only when `TMUX` is set.
    /// `None` when the probe failed, timed out, or was never needed.
    pub tmux_version: Option<String>,
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
            tmux_version: tmux_version_probe(),
        }
    }
}

/// One-shot bounded `tmux -V` probe, run only when the `TMUX` environment
/// variable is set and cached per process so at most one subprocess is ever
/// spawned. The version decides whether Kitty APCs can be sent through tmux's
/// passthrough wrapper without reading from the TUI's shared input stream.
fn tmux_version_probe() -> Option<String> {
    static PROBE: LazyLock<Option<String>> = LazyLock::new(|| {
        nonempty_env("TMUX")?;
        run_tmux_version_command()
    });
    PROBE.clone()
}

/// Run `tmux -V` with a bounded wait. Returns stdout only when the command
/// exits successfully within [`TMUX_VERSION_TIMEOUT`]; a hung or failing
/// binary is treated as "version unknown" and never blocks the caller.
fn run_tmux_version_command() -> Option<String> {
    let mut child = Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + TMUX_VERSION_TIMEOUT;
    loop {
        match child.try_wait().ok()? {
            Some(status) if status.success() => {
                let mut output = String::new();
                child.stdout.take()?.read_to_string(&mut output).ok()?;
                return Some(output);
            }
            Some(_) => return None,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(TMUX_VERSION_POLL),
        }
    }
}

/// Parse `tmux -V` output such as `tmux 3.4`, `tmux 3.3a`, or `tmux next-3.4`
/// into a `(major, minor)` pair. Trailing suffixes (`a`, `-rc`, dates) and a
/// leading `next-` are ignored; unparseable output yields `None`.
#[must_use]
pub fn parse_tmux_version(output: &str) -> Option<(u32, u32)> {
    let mut parts = output.split_whitespace();
    let first = parts.next()?;
    let version = if first == "tmux" { parts.next()? } else { first };
    let version = version.strip_prefix("next-").unwrap_or(version);
    let mut digits = version.split(|c: char| !c.is_ascii_digit());
    let major = digits.next()?.parse().ok()?;
    let minor = digits.next()?.parse().ok()?;
    Some((major, minor))
}

/// Terminals with a partial kitty implementation (Warp) that answer graphics
/// queries but lack the placement/crop/clear guarantees inline overlays rely
/// on. They are explicitly excluded so stale pixels can never linger.
fn is_partial_kitty_terminal(environment: &TerminalEnvironment) -> bool {
    environment
        .term_program
        .as_deref()
        .is_some_and(|program| program.eq_ignore_ascii_case("WarpTerminal"))
}

fn has_kitty_environment_evidence(environment: &TerminalEnvironment) -> bool {
    let term = environment.term.as_deref().unwrap_or_default();
    let term_program = environment.term_program.as_deref().unwrap_or_default();
    environment.kitty_window_id.is_some()
        || term.eq_ignore_ascii_case("xterm-kitty")
        || term_program.eq_ignore_ascii_case("wezterm")
        || term_program.eq_ignore_ascii_case("ghostty")
}

/// Detect protocols from explicit, protocol-specific evidence, with one
/// multiplexer carve-out: tmux 3.3 and newer forward kitty APCs wrapped in
/// its DCS passthrough sequence, so a tmux session at that version reports
/// Kitty. Screen sessions (`STY`) and older tmux keep falling back.
///
/// The interactive TUI deliberately does not perform an active response probe:
/// terminal replies and user keystrokes share the same input stream, and a
/// startup reader cannot consume one without risking the other. The valid
/// query bytes and lossless response splitter below define the contract for
/// any isolated caller that owns its input channel.
#[must_use]
pub fn detect_protocol(environment: &TerminalEnvironment) -> Option<TerminalImageProtocol> {
    if environment.sty.is_some() {
        return None;
    }
    if environment.tmux.is_some() {
        if is_partial_kitty_terminal(environment) || !has_kitty_environment_evidence(environment) {
            return None;
        }
        let version = environment.tmux_version.as_deref().and_then(parse_tmux_version);
        let kitty_passthrough = version.is_some_and(|(major, minor)| {
            major > TMUX_KITTY_SUPPORT.0
                || (major == TMUX_KITTY_SUPPORT.0 && minor >= TMUX_KITTY_SUPPORT.1)
        });
        return kitty_passthrough.then_some(TerminalImageProtocol::Kitty);
    }
    if is_partial_kitty_terminal(environment) {
        // Warp answers kitty queries but lacks the placement/crop/clear
        // guarantees inline overlays rely on; stale pixels could linger.
        return None;
    }

    if has_kitty_environment_evidence(environment) {
        return Some(TerminalImageProtocol::Kitty);
    }
    let term = environment.term.as_deref().unwrap_or_default();
    let term_program = environment.term_program.as_deref().unwrap_or_default();
    if term_program.eq_ignore_ascii_case("iterm.app") && environment.iterm_session_id.is_some() {
        return Some(TerminalImageProtocol::Iterm2);
    }
    if term.to_ascii_lowercase().contains("sixel") {
        return Some(TerminalImageProtocol::Sixel);
    }
    None
}

/// Wrap one complete escape sequence in tmux's DCS passthrough. Every ESC
/// byte is doubled so tmux forwards the inner sequence verbatim to the outer
/// terminal, e.g. `\x1b_G…\x1b\\` becomes
/// `\x1bPtmux;\x1b\x1b_G…\x1b\x1b\\\x1b\\`. Input that is already wrapped
/// passes through unchanged, so emission paths can wrap unconditionally
/// without risking nested passthrough.
#[must_use]
pub fn wrap_tmux_passthrough(bytes: &[u8]) -> Vec<u8> {
    if bytes.starts_with(TMUX_PASSTHROUGH_PREFIX) {
        return bytes.to_vec();
    }
    let mut wrapped = Vec::with_capacity(bytes.len() + TMUX_PASSTHROUGH_PREFIX.len() + TMUX_PASSTHROUGH_SUFFIX.len());
    wrapped.extend_from_slice(TMUX_PASSTHROUGH_PREFIX);
    for &byte in bytes {
        wrapped.push(byte);
        if byte == 0x1b {
            wrapped.push(0x1b);
        }
    }
    wrapped.extend_from_slice(TMUX_PASSTHROUGH_SUFFIX);
    wrapped
}

/// Build the spec query for a caller that owns its input channel. The query
/// sends one 1x1 RGB pixel and asks the terminal not to retain it. The
/// interactive TUI never sends this on shared stdin/stdout during startup.
fn kitty_support_query(tmux_passthrough: bool) -> Vec<u8> {
    if tmux_passthrough {
        wrap_tmux_passthrough(KITTY_SUPPORT_QUERY)
    } else {
        KITTY_SUPPORT_QUERY.to_vec()
    }
}

/// Remove only the complete response for [`KITTY_QUERY_ID`], preserving all
/// unrelated bytes byte-for-byte for the input owner. `Some(true)` is `OK`,
/// `Some(false)` is a terminal error, and `None` means no complete response.
fn split_kitty_support_response(bytes: &[u8]) -> (Option<bool>, Vec<u8>) {
    let prefix = format!("\x1b_Gi={KITTY_QUERY_ID};");
    let Some(start) = bytes
        .windows(prefix.len())
        .position(|window| window == prefix.as_bytes())
    else {
        return (None, bytes.to_vec());
    };
    let payload_start = start + prefix.len();
    let Some(end_offset) = bytes[payload_start..]
        .windows(2)
        .position(|window| window == b"\x1b\\")
    else {
        return (None, bytes.to_vec());
    };
    let end = payload_start + end_offset;
    let mut preserved = Vec::with_capacity(bytes.len() - (end + 2 - start));
    preserved.extend_from_slice(&bytes[..start]);
    preserved.extend_from_slice(&bytes[end + 2..]);
    (Some(&bytes[payload_start..end] == b"OK"), preserved)
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
    /// Wrap every kitty APC in tmux's DCS passthrough. Set when the TUI
    /// writes through tmux; cursor moves and iTerm/sixel output stay plain.
    tmux_passthrough: bool,
    metadata: VecDeque<MetadataCacheEntry>,
    /// Encoded PNG per asset (exact display pixels), reused by every
    /// placement of that asset. Evicted when the asset leaves the frame set.
    prepared: HashMap<AssetKey, PreparedImage>,
    /// Assets transmitted to the terminal: asset -> collision-resistant image
    /// id. Each asset is transmitted exactly once while it remains live.
    transmitted: HashMap<AssetKey, u32>,
    /// Exact placements drawn in the current frame. Explicit placement ids let
    /// reconciliation remove or replace only this renderer's overlays.
    active: HashMap<RenderIdentity, ActiveKittyPlacement>,
    frame_identity: Option<ImageFrameIdentity>,
    next_kitty_id: u32,
}

impl Default for TerminalImageRenderer {
    fn default() -> Self {
        let environment = TerminalEnvironment::current();
        Self::with_tmux_passthrough(detect_protocol(&environment), environment.tmux.is_some())
    }
}

impl TerminalImageRenderer {
    #[must_use]
    pub fn new(protocol: Option<TerminalImageProtocol>) -> Self {
        Self::with_tmux_passthrough(protocol, false)
    }

    /// Construct with an explicit protocol and tmux passthrough state, so
    /// tests and embedders exercise DCS wrapping without consulting the
    /// process environment.
    #[must_use]
    pub fn with_tmux_passthrough(
        protocol: Option<TerminalImageProtocol>,
        tmux_passthrough: bool,
    ) -> Self {
        Self {
            protocol,
            tmux_passthrough,
            metadata: VecDeque::new(),
            prepared: HashMap::new(),
            transmitted: HashMap::new(),
            active: HashMap::new(),
            frame_identity: None,
            next_kitty_id: random_nonzero_kitty_id(),
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
    /// Kitty assets are transmitted once and then placed by id, so scrolling
    /// and repeated placements reuse the payload instead of retransmitting.
    pub fn present<W: Write>(
        &mut self,
        writer: &mut W,
        frame: ImageFrameIdentity,
        placements: &[ImagePlacement],
    ) -> io::Result<()> {
        if !self.supports_images() {
            self.active.clear();
            self.prepared.clear();
            self.transmitted.clear();
            self.frame_identity = Some(frame);
            return Ok(());
        }

        self.frame_identity = Some(frame);

        let desired = placements
            .iter()
            .map(RenderIdentity::from)
            .collect::<HashSet<_>>();
        let stale = self
            .active
            .keys()
            .filter(|identity| !desired.contains(identity))
            .copied()
            .collect::<Vec<_>>();
        for identity in stale {
            let active = self
                .active
                .remove(&identity)
                .expect("stale placement came from active map");
            if self.protocol == Some(TerminalImageProtocol::Kitty) {
                write_kitty_delete_placement(
                    writer,
                    active.image_id,
                    active.placement_id,
                    self.tmux_passthrough,
                )?;
            }
        }
        let live_assets = desired
            .iter()
            .map(|identity| identity.asset)
            .collect::<HashSet<_>>();
        let orphaned = self
            .transmitted
            .iter()
            .filter_map(|(asset, id)| (!live_assets.contains(asset)).then_some((*asset, *id)))
            .collect::<Vec<_>>();
        for (asset, id) in orphaned {
            write_kitty_delete(writer, id, self.tmux_passthrough)?;
            self.transmitted.remove(&asset);
        }
        self.prepared
            .retain(|asset, _| live_assets.contains(asset));

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
            let (kitty_id, placement_id) = match self.protocol {
                Some(TerminalImageProtocol::Kitty) => {
                    let image_id = if let Some(&id) = self.transmitted.get(&placement.layout.asset) {
                        id
                    } else {
                        let id = self.allocate_kitty_id();
                        let prepared = self
                            .prepared
                            .get(&placement.layout.asset)
                            .expect("prepared image inserted above");
                        write_kitty_transmit(
                            writer,
                            id,
                            placement.layout.asset,
                            &prepared.png_base64,
                            self.tmux_passthrough,
                        )?;
                        self.transmitted.insert(placement.layout.asset, id);
                        id
                    };
                    (image_id, self.allocate_kitty_id())
                }
                Some(TerminalImageProtocol::Iterm2 | TerminalImageProtocol::Sixel) | None => (0, 0),
            };
            write!(
                writer,
                "\x1b7\x1b[{};{}H",
                placement.y.saturating_add(1),
                placement.x.saturating_add(1)
            )?;
            match self.protocol {
                Some(TerminalImageProtocol::Kitty) => {
                    write_kitty_place(
                        writer,
                        kitty_id,
                        placement_id,
                        placement.layout,
                        self.tmux_passthrough,
                    )?;
                }
                Some(TerminalImageProtocol::Iterm2) => {
                    let prepared = self
                        .prepared
                        .get(&placement.layout.asset)
                        .expect("prepared image inserted above");
                    write_iterm_image(writer, placement.layout, &prepared.png_base64)?;
                }
                Some(TerminalImageProtocol::Sixel) | None => {}
            }
            writer.write_all(b"\x1b8")?;
            self.active.insert(identity, ActiveKittyPlacement {
                image_id: kitty_id,
                placement_id,
            });
        }
        writer.flush()
    }

    /// Delete this renderer's transmitted Kitty image IDs before terminal
    /// release. No global delete is used, so other applications remain intact.
    pub fn cleanup<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.delete_transmitted_kitty(writer)?;
        self.active.clear();
        self.prepared.clear();
        self.transmitted.clear();
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
        loop {
            let id = self.next_kitty_id.max(1);
            self.next_kitty_id = id.wrapping_add(1).max(1);
            let image_id_is_live = self.transmitted.values().any(|candidate| *candidate == id);
            let placement_id_is_live = self.active.values().any(|active| active.placement_id == id);
            if !image_id_is_live && !placement_id_is_live {
                return id;
            }
        }
    }

    fn delete_transmitted_kitty<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        if self.protocol == Some(TerminalImageProtocol::Kitty) {
            for id in self.transmitted.values().copied().filter(|id| *id != 0) {
                write_kitty_delete(writer, id, self.tmux_passthrough)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct ActiveKittyPlacement {
    image_id: u32,
    placement_id: u32,
}

fn random_nonzero_kitty_id() -> u32 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    u32::from_le_bytes(bytes[..4].try_into().expect("UUID has four leading bytes")).max(1)
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

fn write_kitty_escape<W: Write>(writer: &mut W, passthrough: bool, escape: &[u8]) -> io::Result<()> {
    if passthrough {
        writer.write_all(&wrap_tmux_passthrough(escape))
    } else {
        writer.write_all(escape)
    }
}

/// Transmit one encoded PNG under `id`. The transmit-only action stores the
/// image; later `a=p` commands with explicit placement ids draw it.
fn write_kitty_transmit<W: Write>(
    writer: &mut W,
    id: u32,
    asset: AssetKey,
    payload: &str,
    passthrough: bool,
) -> io::Result<()> {
    let mut chunks = payload.as_bytes().chunks(KITTY_CHUNK_BYTES).peekable();
    let Some(first) = chunks.next() else {
        return Ok(());
    };
    let mut escape = Vec::with_capacity(first.len() + 64);
    let more = u8::from(chunks.peek().is_some());
    write!(
        escape,
        "\x1b_Ga=t,f=100,t=d,i={id},q=2,s={},v={},m={more};",
        asset.width_pixels, asset.height_pixels
    )?;
    escape.extend_from_slice(first);
    escape.extend_from_slice(b"\x1b\\");
    write_kitty_escape(writer, passthrough, &escape)?;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        escape.clear();
        write!(escape, "\x1b_Gm={more};")?;
        escape.extend_from_slice(chunk);
        escape.extend_from_slice(b"\x1b\\");
        write_kitty_escape(writer, passthrough, &escape)?;
    }
    Ok(())
}

/// Place a previously transmitted image at the current cursor position. The
/// explicit placement id makes updates and deletion ownership-scoped.
fn write_kitty_place<W: Write>(
    writer: &mut W,
    id: u32,
    placement_id: u32,
    layout: ImageLayout,
    passthrough: bool,
) -> io::Result<()> {
    let mut escape = Vec::with_capacity(48);
    write!(
        escape,
        "\x1b_Ga=p,i={id},p={placement_id},q=2,c={},r={}\x1b\\",
        layout.columns, layout.rows
    )?;
    write_kitty_escape(writer, passthrough, &escape)
}

fn write_kitty_delete<W: Write>(writer: &mut W, id: u32, passthrough: bool) -> io::Result<()> {
    let mut escape = Vec::with_capacity(32);
    write!(escape, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
    write_kitty_escape(writer, passthrough, &escape)
}

fn write_kitty_delete_placement<W: Write>(
    writer: &mut W,
    image_id: u32,
    placement_id: u32,
    passthrough: bool,
) -> io::Result<()> {
    let mut escape = Vec::with_capacity(48);
    write!(escape, "\x1b_Ga=d,d=i,i={image_id},p={placement_id},q=2\x1b\\")?;
    write_kitty_escape(writer, passthrough, &escape)
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
    fn non_tmux_kitty_evidence_covers_wezterm_and_ghostty() {
        for term_program in ["wezterm", "ghostty", "WezTerm", "Ghostty"] {
            let environment = TerminalEnvironment {
                term_program: Some(term_program.to_owned()),
                ..TerminalEnvironment::default()
            };
            assert_eq!(
                detect_protocol(&environment),
                Some(TerminalImageProtocol::Kitty),
                "TERM_PROGRAM={term_program:?}"
            );
        }
    }

    #[test]
    fn tmux_version_parser_handles_releases_suffixes_and_garbage() {
        assert_eq!(parse_tmux_version("tmux 3.4"), Some((3, 4)));
        assert_eq!(parse_tmux_version("tmux 3.3a"), Some((3, 3)));
        assert_eq!(parse_tmux_version("tmux 3.2"), Some((3, 2)));
        assert_eq!(parse_tmux_version("tmux 2.9a"), Some((2, 9)));
        assert_eq!(parse_tmux_version("tmux next-3.4"), Some((3, 4)));
        assert_eq!(parse_tmux_version("3.4"), Some((3, 4)));
        assert_eq!(parse_tmux_version(""), None);
        assert_eq!(parse_tmux_version("tmux"), None);
        assert_eq!(parse_tmux_version("garbage"), None);
    }

    #[test]
    fn tmux_detection_requires_version_3_3_or_newer() {
        let kitty = TerminalEnvironment {
            kitty_window_id: Some("1".to_owned()),
            ..TerminalEnvironment::default()
        };
        for (version, expected) in [
            (Some("tmux 3.3a"), Some(TerminalImageProtocol::Kitty)),
            (Some("tmux 3.4"), Some(TerminalImageProtocol::Kitty)),
            (Some("tmux next-3.4"), Some(TerminalImageProtocol::Kitty)),
            (Some("tmux 3.2"), None),
            (Some("tmux 2.9a"), None),
            (Some("not a version"), None),
            (None, None), // version probe failed, binary missing, or offline
        ] {
            let tmux = TerminalEnvironment {
                tmux: Some("/tmp/tmux".to_owned()),
                tmux_version: version.map(str::to_owned),
                ..kitty.clone()
            };
            assert_eq!(
                detect_protocol(&tmux),
                expected,
                "tmux_version={version:?}"
            );
        }
        let tmux_without_outer_terminal_evidence = TerminalEnvironment {
            tmux: Some("/tmp/tmux".to_owned()),
            tmux_version: Some("tmux 3.4".to_owned()),
            ..TerminalEnvironment::default()
        };
        assert_eq!(
            detect_protocol(&tmux_without_outer_terminal_evidence),
            None,
            "tmux version alone must not claim an unknown outer terminal supports Kitty"
        );
        let sty = TerminalEnvironment {
            sty: Some("screen".to_owned()),
            ..kitty
        };
        assert_eq!(detect_protocol(&sty), None);
    }

    #[test]
    fn tmux_passthrough_wrapper_uses_dcs_and_doubles_escapes() {
        let escape = b"\x1b_Ga=t,f=100,m=0;AAAA\x1b\\";
        assert_eq!(
            wrap_tmux_passthrough(escape),
            b"\x1bPtmux;\x1b\x1b_Ga=t,f=100,m=0;AAAA\x1b\x1b\\\x1b\\"
        );
        let once = wrap_tmux_passthrough(escape);
        assert_eq!(
            wrap_tmux_passthrough(&once),
            once,
            "already-wrapped input must not be re-wrapped"
        );
        assert!(once.starts_with(b"\x1bPtmux;"));
    }


    /// Strip tmux's DCS passthrough wrapper the same way tmux does: consume
    /// doubled ESC pairs inside the payload and terminate on the first
    /// unescaped `ESC \`.
    fn unwrap_tmux_passthrough(wrapped: &str) -> String {
        let mut out = String::new();
        let mut rest = wrapped;
        while let Some(prefix) = rest.find("\u{1b}Ptmux;") {
            out.push_str(&rest[..prefix]);
            rest = &rest[prefix + "\u{1b}Ptmux;".len()..];
            let bytes = rest.as_bytes();
            let mut i = 0;
            let end = loop {
                assert!(
                    i + 1 < bytes.len(),
                    "unterminated tmux passthrough segment"
                );
                if bytes[i] == 0x1b {
                    if bytes[i + 1] == 0x1b {
                        i += 2;
                        continue;
                    }
                    if bytes[i + 1] == b'\\' {
                        break i;
                    }
                }
                i += 1;
            };
            out.push_str(&rest[..end].replace("\u{1b}\u{1b}", "\u{1b}"));
            rest = &rest[end + 2..];
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn tmux_passthrough_wraps_each_kitty_apc_once_and_leaves_cursor_escapes_plain() {
        let data = png(160, 160);
        let config = ImageDisplayConfig {
            show_images: true,
            width_cells: 20,
        };
        let layout = TerminalImageRenderer::new(Some(TerminalImageProtocol::Kitty))
            .layout(
                &data,
                "image/png",
                config,
                80,
                24,
                TerminalCellSize::default(),
            )
            .unwrap();
        let placement = ImagePlacement::new(layout, &data, "image/png", 3, 4);

        let mut plain = TerminalImageRenderer::new(Some(TerminalImageProtocol::Kitty));
        let mut plain_bytes = Vec::new();
        plain
            .present(&mut plain_bytes, frame(1), std::slice::from_ref(&placement))
            .unwrap();
        let plain_out = String::from_utf8(plain_bytes).unwrap();

        let mut wrapped =
            TerminalImageRenderer::with_tmux_passthrough(Some(TerminalImageProtocol::Kitty), true);
        let wrapped_placement = ImagePlacement::new(layout, &data, "image/png", 3, 4);
        let mut wrapped_bytes = Vec::new();
        wrapped
            .present(
                &mut wrapped_bytes,
                frame(1),
                std::slice::from_ref(&wrapped_placement),
            )
            .unwrap();
        let wrapped_out = String::from_utf8(wrapped_bytes).unwrap();

        assert!(
            wrapped_out.starts_with("\u{1b}Ptmux;\u{1b}\u{1b}_Ga=t"),
            "the transmit is wrapped and comes first: {wrapped_out:?}"
        );
        assert!(
            wrapped_out.ends_with("\u{1b}8"),
            "cursor restore stays outside the DCS wrapper"
        );
        assert_eq!(
            wrapped_out.matches("\u{1b}Ptmux;").count(),
            plain_out.matches("\u{1b}_G").count(),
            "every kitty APC is wrapped exactly once"
        );
        assert!(
            !wrapped_out.contains("\u{1b}Ptmux;\u{1b}Ptmux;"),
            "no nested passthrough"
        );

        let mut cleanup = Vec::new();
        wrapped.cleanup(&mut cleanup).unwrap();
        let cleanup = String::from_utf8(cleanup).unwrap();
        assert!(
            cleanup.contains("\u{1b}Ptmux;\u{1b}\u{1b}_Ga=d,d=I,i="),
            "cleanup deletes are wrapped too: {cleanup:?}"
        );
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
        // A spec-valid transmit-only command comes first. Width and height use
        // separate control keys; the following placement has an explicit id.
        assert!(first.starts_with("\u{1b}_Ga=t,f=100,t=d,i="));
        assert!(first.contains(",q=2,s=160,v=160,"));
        assert!(!first.contains("a=T"));
        assert!(!first.contains("s=160x160"));
        assert!(
            first.contains("\u{1b}7\u{1b}[5;4H\u{1b}_Ga=p,i=")
                && first.contains(",p=")
                && first.contains(",q=2,c=20,r=10"),
            "one explicit placement draws the transmitted image at the cursor"
        );
        for protocol_frame in first.split("\u{1b}\\") {
            // Each Kitty chunk is `\x1b_G<control>;<payload>`. The cursor
            // positioning CSI inside the placement segment also contains a
            // `;`, so isolate the payload by splitting after the `\x1b_G`
            // APC introducer rather than on the first `;` in the segment.
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

        // A new frame with the same placement is already reconciled and emits
        // no duplicate placement.
        let mut changed = Vec::new();
        renderer
            .present(&mut changed, frame(2), std::slice::from_ref(&placement))
            .unwrap();
        assert!(changed.is_empty(), "unchanged placements survive frame identity changes");

        // When the asset leaves the frame set, its exact placement and image
        // id are deleted without touching another client's images.
        let mut gone = Vec::new();
        renderer.present(&mut gone, frame(3), &[]).unwrap();
        let gone = String::from_utf8(gone).unwrap();
        assert!(gone.contains("a=d,d=i,i="));
        assert!(gone.contains(",p="));
        assert!(gone.contains("a=d,d=I,i="));
        assert!(!gone.contains("d=A"));

        let mut again = Vec::new();
        renderer
            .present(&mut again, frame(4), std::slice::from_ref(&placement))
            .unwrap();
        let again = String::from_utf8(again).unwrap();
        assert!(again.contains("a=t,f=100,t=d,i="));

        let mut cleanup = Vec::new();
        renderer.cleanup(&mut cleanup).unwrap();
        let cleanup = String::from_utf8(cleanup).unwrap();
        assert!(cleanup.contains("a=d,d=I,i="));
        assert!(!cleanup.contains("d=A"));
    }

    #[test]
    fn kitty_transmits_once_and_places_many_for_shared_assets() {
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
        let placements = [
            ImagePlacement::new(layout, &data, "image/png", 1, 1),
            ImagePlacement::new(layout, &data, "image/png", 40, 1),
            ImagePlacement::new(layout, &data, "image/png", 1, 12),
        ];
        let mut output = Vec::new();
        renderer.present(&mut output, frame(1), &placements).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            output.matches("a=t,").count(),
            1,
            "one transmit serves every placement"
        );
        assert_eq!(output.matches("a=p,i=").count(), 3);
        let placement_ids = output
            .split("a=p,i=")
            .skip(1)
            .filter_map(|command| command.split(",p=").nth(1))
            .filter_map(|command| command.split(',').next())
            .collect::<HashSet<_>>();
        assert_eq!(placement_ids.len(), 3, "each placement has an isolated id");
        assert!(!output.contains("a=d"), "no deletions while the asset is live");
        assert_eq!(output.matches("\u{1b}7").count(), 3, "cursor save per placement");
    }

    #[test]
    fn kitty_query_is_valid_and_response_split_preserves_user_input() {
        assert_eq!(
            kitty_support_query(false),
            b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"
        );
        assert_eq!(
            kitty_support_query(true),
            b"\x1bPtmux;\x1b\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\x1b\\\x1b\\"
        );

        let input = b"typed-before\x1b_Gi=31;OK\x1b\\typed-after";
        let (supported, preserved) = split_kitty_support_response(input);
        assert_eq!(supported, Some(true));
        assert_eq!(preserved, b"typed-beforetyped-after");

        let incomplete = b"x\x1b_Gi=31;O";
        let (supported, preserved) = split_kitty_support_response(incomplete);
        assert_eq!(supported, None);
        assert_eq!(preserved, incomplete);

        let other = b"x\x1b_Gi=99;OK\x1b\\y";
        let (supported, preserved) = split_kitty_support_response(other);
        assert_eq!(supported, None);
        assert_eq!(preserved, other);
    }

    #[test]
    fn kitty_reposition_deletes_old_explicit_placement_only() {
        let data = png(16, 16);
        let mut renderer = TerminalImageRenderer::new(Some(TerminalImageProtocol::Kitty));
        let layout = renderer
            .layout(&data, "image/png", ImageDisplayConfig { show_images: true, width_cells: 4 }, 80, 24, TerminalCellSize::default())
            .unwrap();
        let first = ImagePlacement::new(layout, &data, "image/png", 1, 1);
        let moved = ImagePlacement::new(layout, &data, "image/png", 20, 8);
        let mut initial = Vec::new();
        renderer.present(&mut initial, frame(1), &[first]).unwrap();
        let initial = String::from_utf8(initial).unwrap();
        let image_id = initial.split("a=t,").nth(1).unwrap().split("i=").nth(1).unwrap().split(',').next().unwrap();
        let old_placement_id = initial.split("a=p,i=").nth(1).unwrap().split(",p=").nth(1).unwrap().split(',').next().unwrap();

        let mut repositioned = Vec::new();
        renderer.present(&mut repositioned, frame(2), &[moved]).unwrap();
        let repositioned = String::from_utf8(repositioned).unwrap();
        assert!(repositioned.contains(&format!("a=d,d=i,i={image_id},p={old_placement_id},q=2")));
        assert!(repositioned.contains(&format!("a=p,i={image_id},p=")));
        assert!(repositioned.contains("\u{1b}[9;21H"));
        assert!(!repositioned.contains("a=t,"), "moving reuses transmitted data");
        assert!(!repositioned.contains("d=A"));
    }

    #[test]
    fn kitty_renderers_use_collision_resistant_image_and_placement_ids() {
        let data = png(8, 8);
        let render_ids = || {
            let mut renderer = TerminalImageRenderer::new(Some(TerminalImageProtocol::Kitty));
            let layout = renderer.layout(&data, "image/png", ImageDisplayConfig { show_images: true, width_cells: 2 }, 80, 24, TerminalCellSize::default()).unwrap();
            let mut bytes = Vec::new();
            renderer.present(&mut bytes, frame(1), &[ImagePlacement::new(layout, &data, "image/png", 0, 0)]).unwrap();
            let output = String::from_utf8(bytes).unwrap();
            let image_id = output.split("a=t,").nth(1).unwrap().split("i=").nth(1).unwrap().split(',').next().unwrap().parse::<u32>().unwrap();
            let placement_id = output.split("a=p,i=").nth(1).unwrap().split(",p=").nth(1).unwrap().split(',').next().unwrap().parse::<u32>().unwrap();
            (image_id, placement_id)
        };
        let first = render_ids();
        let mut second = render_ids();
        for _ in 0..3 {
            if first != second {
                break;
            }
            second = render_ids();
        }
        assert_ne!(first.0, 0);
        assert_ne!(first.1, 0);
        assert_ne!(first.0, first.1);
        assert_ne!(first, second, "independent renderer namespaces must not start at fixed ids");
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
