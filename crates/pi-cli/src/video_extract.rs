//! Bounded, deterministic video preprocessing shared by the Web control
//! plane (the `POST /upload/video` endpoint in `modes::listen`) and the TUI
//! (`@clip.mkv` file expansion in `file_args`).
//!
//! Raw video bytes never enter a `ContentBlock` and never ride the prompt
//! WebSocket JSON. Every upload is validated server-side (sanitized file
//! name, container magic, actual media stream) and reduced to a small
//! chronological set of JPEG frames that re-use the existing image
//! `ContentBlock` path, plus a bounded instruction/marker naming the
//! sanitized file, its duration, and the frame timestamps in order.
//!
//! The only external tool is `ffmpeg` — used for BOTH probing and frame
//! extraction, so `ffprobe` is not required. It is spawned directly with
//! argv (never through a shell) under strict byte, duration, frame, and
//! wall-clock bounds; stderr is captured with a hard cap, its diagnostics
//! are path-scrubbed and secret-redacted before any error surface. All
//! temporary artifacts live in a request-scoped random directory that is
//! removed on drop; nothing persists server-side.

use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use http::StatusCode;
use pi_ai::ContentBlock;
use serde::Serialize;
use uuid::Uuid;

use base64::Engine as _;

#[cfg(test)]
use std::future::Future;

/// Hard cap for one uploaded video's raw bytes. The listener checks the
/// `Content-Length` against this before reading any body byte, and
/// [`extract_video`] re-checks the bytes as defense in depth.
pub(crate) const MAX_VIDEO_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
/// Longest accepted video duration; longer files are rejected after the
/// probe so frame extraction stays bounded.
pub(crate) const MAX_VIDEO_DURATION_SECONDS: f64 = 600.0;
/// Deterministic chronological sample count. Frames are requested at
/// `fps = max_frames / duration`, so a 12.34 s video yields 6 frames at
/// approximately 0.00 s, 2.06 s, 4.11 s, 6.17 s, 8.23 s, 10.28 s.
pub(crate) const MAX_VIDEO_FRAMES: usize = 6;
/// Per-frame JPEG raw-byte cap after extraction.
pub(crate) const MAX_FRAME_JPEG_BYTES: usize = 384 * 1024;
/// Total raw JPEG bytes across all frames of one attachment (keeps the
/// base64 payload comfortably inside the 4 MiB prompt RPC frame cap even
/// alongside the user's own text and image attachments).
pub(crate) const MAX_FRAMES_TOTAL_BYTES: usize = 2 * 1024 * 1024;
/// Wall-clock bound for the probe pass.
pub(crate) const VIDEO_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// Wall-clock bound for the extraction pass.
pub(crate) const VIDEO_EXTRACT_TIMEOUT: Duration = Duration::from_secs(60);
/// ffmpeg stderr capture cap; a process whose diagnostics exceed this is
/// treated as pathological and killed rather than stalling the request.
const MAX_FFMPEG_STDERR_BYTES: usize = 64 * 1024;
/// Cap for the generated instruction/marker text.
const MAX_VIDEO_INSTRUCTION_CHARS: usize = 800;
/// Cap for the sanitized display name (characters, excluding the extension).
const MAX_VIDEO_NAME_CHARS: usize = 120;
/// Frame JPEG width; height keeps the aspect ratio and is rounded to even.
/// `min()` keeps already-small videos from being upscaled.
const FRAME_SCALE_WIDTH: u32 = 640;
/// ffmpeg JPEG quality (`-q:v`, smaller = better).
const FRAME_JPEG_QUALITY: u8 = 3;
/// Leading bytes required for a cheap container-consistency check.
const VIDEO_SNIFF_BYTES: usize = 64;

/// Supported video containers: (extension, canonical MIME type). The
/// extension is validated BEFORE sanitization; the sanitized name is a
/// display string only and is never used to build a filesystem path.
pub(crate) const VIDEO_CONTAINERS: &[(&str, &str)] = &[
    ("mkv", "video/x-matroska"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("mov", "video/quicktime"),
    ("avi", "video/x-msvideo"),
    ("ogg", "video/ogg"),
];

/// All bounds for one video preprocessing run. Production uses
/// [`VideoLimits::default`]; tests shrink the windows so timeouts and byte
/// caps are provable without waiting out the real durations.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VideoLimits {
    pub upload_bytes: usize,
    pub duration_seconds: f64,
    pub max_frames: usize,
    pub frame_jpeg_bytes: usize,
    pub frames_total_bytes: usize,
    pub probe_timeout: Duration,
    pub extract_timeout: Duration,
}

impl VideoLimits {
    pub(crate) const fn default() -> Self {
        Self {
            upload_bytes: MAX_VIDEO_UPLOAD_BYTES,
            duration_seconds: MAX_VIDEO_DURATION_SECONDS,
            max_frames: MAX_VIDEO_FRAMES,
            frame_jpeg_bytes: MAX_FRAME_JPEG_BYTES,
            frames_total_bytes: MAX_FRAMES_TOTAL_BYTES,
            probe_timeout: VIDEO_PROBE_TIMEOUT,
            extract_timeout: VIDEO_EXTRACT_TIMEOUT,
        }
    }
}

/// Classified failure of a video preprocessing run, mapped to an HTTP status
/// by the Web endpoint and rendered as bounded actionable text everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoErrorKind {
    /// ffmpeg is not installed / not on PATH.
    FfmpegMissing,
    /// File name extension not allowed, or content does not match the
    /// declared container.
    UnsupportedType,
    /// Uploaded bytes over the cap.
    TooLarge,
    /// Probed duration over the cap.
    DurationTooLong,
    /// Probe or extraction exceeded its wall-clock bound.
    Timeout,
    /// The file is not a decodable video (no stream, corrupt, unreadable).
    InvalidMedia,
    /// Internal I/O failure (staging, work directory, reading frames).
    Internal,
}

#[derive(Debug)]
pub(crate) struct VideoExtractError {
    pub kind: VideoErrorKind,
    pub message: String,
}

impl VideoExtractError {
    fn new(kind: VideoErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn status(&self) -> StatusCode {
        match self.kind {
            VideoErrorKind::FfmpegMissing => StatusCode::SERVICE_UNAVAILABLE,
            VideoErrorKind::UnsupportedType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            VideoErrorKind::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            VideoErrorKind::DurationTooLong => StatusCode::UNPROCESSABLE_ENTITY,
            VideoErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
            VideoErrorKind::InvalidMedia | VideoErrorKind::Internal => StatusCode::BAD_REQUEST,
        }
    }
}

/// One extracted JPEG frame. The `data` payload is base64 — exactly the
/// shape of the existing image `ContentBlock`, so the Web client can pass
/// the frame array straight into `prompt`/`steer` `images`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoFrame {
    /// Chronological order within the attachment (0-based).
    pub index: usize,
    /// Approximate position in the source video (seconds), `index * duration / max_frames`.
    pub timestamp_seconds: f64,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    /// Raw JPEG byte length (before base64).
    pub size_bytes: usize,
    /// Base64-encoded JPEG.
    pub data: String,
}

/// Result of a successful preprocessing run: sanitized identity plus the
/// chronological JPEG frames and the bounded instruction/marker.
#[derive(Clone, Debug)]
pub(crate) struct ExtractedVideo {
    /// Sanitized display name including the (lowercased) extension.
    pub name: String,
    pub container: &'static str,
    pub mime_type: &'static str,
    pub size_bytes: usize,
    pub duration_seconds: f64,
    pub frames: Vec<VideoFrame>,
    pub instruction: String,
}

impl ExtractedVideo {
    /// Total base64 length of every frame, for client-side budgeting.
    pub(crate) fn frames_base64_bytes(&self) -> usize {
        self.frames.iter().map(|frame| frame.data.len()).sum()
    }

    /// The frames as existing image `ContentBlock`s, chronological order.
    pub(crate) fn into_content_blocks(&self) -> Vec<ContentBlock> {
        self.frames
            .iter()
            .map(|frame| ContentBlock::Image {
                data: frame.data.clone(),
                mime_type: frame.mime_type.clone(),
            })
            .collect()
    }
}

/// Validate and sanitize a user-supplied video file name. Returns the
/// sanitized display name (bounded, safe characters, original extension
/// lowercased) plus the matched container and MIME. Returns `None` for
/// anything without one of the supported extensions.
///
/// The result is a DISPLAY string only — it is never used to build a
/// filesystem path. It is always the basename (last segment split on both
/// `/` and `\`), so a user-supplied absolute or relative path can never
/// project directory structure into the marker; the stem is additionally
/// scrubbed of every non-alphanumeric character except space, `-`, `_`,
/// and `.`.
pub(crate) fn sanitize_video_name(raw: &str) -> Option<(String, &'static str, &'static str)> {
    if raw.is_empty() || raw.len() > 4096 {
        return None;
    }
    // Basename only: last segment after any '/' or '\' separator, so neither
    // absolute paths nor relative directory structure can leak into the
    // sanitized display name.
    let raw = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let (stem, extension) = raw.rsplit_once('.')?;
    let extension = extension.to_ascii_lowercase();
    let (container, mime_type) = VIDEO_CONTAINERS
        .iter()
        .find(|(candidate, _)| *candidate == extension)?;
    let stem: String = stem
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.') {
                character
            } else {
                ' '
            }
        })
        .collect();
    // Collapse whitespace runs (this also trims leading/trailing blanks).
    let stem = stem.split_whitespace().collect::<Vec<_>>().join(" ");
    let stem = if stem.is_empty() { "video" } else { &stem };
    let stem: String = stem
        .chars()
        .take(MAX_VIDEO_NAME_CHARS.saturating_sub(extension.len() + 1))
        .collect();
    let name = format!("{stem}.{extension}");
    Some((name, container, mime_type))
}

/// Cheap container-consistency check against the leading magic bytes. The
/// ffmpeg probe remains the authority on decodability.
pub(crate) fn container_matches(container: &str, prefix: &[u8]) -> bool {
    match container {
        "mkv" | "webm" => prefix.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]), // EBML
        "mp4" | "mov" => prefix.get(4..8) == Some(b"ftyp"),
        "avi" => prefix.get(0..4) == Some(b"RIFF") && prefix.get(8..12) == Some(b"AVI "),
        "ogg" => prefix.starts_with(b"OggS"),
        _ => false,
    }
}

/// Read the JPEG dimensions from a SOF marker without decoding the image.
/// Returns `None` for anything that is not a structurally valid JPEG.
pub(crate) fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut pos = 2usize; // skip the SOI marker
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            return None;
        }
        let marker = data[pos + 1];
        // SOF0..SOF15 (baseline, extended, progressive, lossless, ...)
        // excluding DHT (0xC4), JPG (0xC8), DAC (0xCC).
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            if pos + 9 > data.len() {
                return None;
            }
            let height = u16::from_be_bytes([data[pos + 5], data[pos + 6]]) as u32;
            let width = u16::from_be_bytes([data[pos + 7], data[pos + 8]]) as u32;
            return Some((width, height));
        }
        if matches!(marker, 0xD9 | 0xDA) {
            // EOI or SOS reached before any SOF: not a decodable image.
            return None;
        }
        if matches!(marker, 0xD0..=0xD7) || marker == 0x01 {
            // Restart markers and TEM carry no length.
            pos += 2;
            continue;
        }
        if pos + 4 > data.len() {
            return None;
        }
        let length = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        if length < 2 {
            return None;
        }
        pos += 2 + length;
    }
    None
}

/// Build the bounded instruction/marker naming the sanitized file, its
/// duration, and the chronological frame timestamps in order.
pub(crate) fn build_instruction(
    name: &str,
    container: &str,
    duration_seconds: f64,
    frames: &[VideoFrame],
) -> String {
    let stamps = frames
        .iter()
        .map(|frame| format!("{:.2}s", frame.timestamp_seconds))
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!(
        "[Video attachment: {name} ({container} container, {duration_seconds:.2}s) — {} chronological JPEG frames sampled at approximately {stamps}; analyze them in order as a sequence of the same video.]",
        frames.len(),
    );
    text.chars().take(MAX_VIDEO_INSTRUCTION_CHARS).collect()
}

/// Resolve the ffmpeg executable. Production uses `ffmpeg` resolved through
/// PATH; tests inject a fake executable via [`with_ffmpeg_program`].
pub(crate) fn ffmpeg_program() -> PathBuf {
    FFMPEG_PROGRAM
        .lock()
        .expect("ffmpeg program lock")
        .clone()
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

static FFMPEG_PROGRAM: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));

/// Serializes video tests that install an ffmpeg override, so concurrent
/// overrides never interleave. This lock is NEVER touched by the production
/// read path ([`ffmpeg_program`]), so holding it across an `.await` cannot
/// deadlock extraction.
#[cfg(test)]
static FFMPEG_TEST_GUARD: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[cfg(test)]
struct FfmpegOverrideGuard {
    previous: Option<PathBuf>,
}

#[cfg(test)]
impl Drop for FfmpegOverrideGuard {
    fn drop(&mut self) {
        *FFMPEG_PROGRAM.lock().expect("ffmpeg program lock") = self.previous.take();
    }
}

#[cfg(test)]
fn set_ffmpeg_override(program: PathBuf) -> FfmpegOverrideGuard {
    let mut guard = FFMPEG_PROGRAM.lock().expect("ffmpeg program lock");
    let previous = guard.replace(program);
    FfmpegOverrideGuard { previous }
}

/// Test-only override of the ffmpeg executable for the duration of `run`.
#[cfg(test)]
pub(crate) fn with_ffmpeg_program<T>(program: PathBuf, run: impl FnOnce() -> T) -> T {
    let _serial = FFMPEG_TEST_GUARD.lock().expect("serialize video test overrides");
    let _override = set_ffmpeg_override(program);
    run()
}

/// Test-only override of the ffmpeg executable for the duration of the
/// async `run` (e.g. the Web upload endpoint handler).
#[cfg(test)]
pub(crate) async fn with_ffmpeg_program_async<T>(
    program: PathBuf,
    run: impl Future<Output = T>,
) -> T {
    let _serial = FFMPEG_TEST_GUARD.lock().expect("serialize video test overrides");
    let _override = set_ffmpeg_override(program);
    run.await
}

/// Run the full validation + extraction pipeline over `video_bytes`.
///
/// Returns [`VideoExtractError`] with a bounded, path-scrubbed, actionable
/// message on any failure. On success every temporary artifact is already
/// deleted (the work directory is dropped before returning); the returned
/// frames live only in memory.
pub(crate) fn extract_video(
    program: &Path,
    limits: VideoLimits,
    video_bytes: Vec<u8>,
    display_name: &str,
) -> Result<ExtractedVideo, VideoExtractError> {
    let Some((name, container, mime_type)) = sanitize_video_name(display_name) else {
        // Generic message: the raw name may carry a client-side local path
        // and must never be echoed back.
        return Err(VideoExtractError::new(
            VideoErrorKind::UnsupportedType,
            "unsupported video file — supported containers: mkv, mp4, webm, mov, avi, ogg",
        ));
    };
    if video_bytes.is_empty() {
        return Err(VideoExtractError::new(
            VideoErrorKind::InvalidMedia,
            "uploaded file is empty",
        ));
    }
    if video_bytes.len() > limits.upload_bytes {
        return Err(VideoExtractError::new(
            VideoErrorKind::TooLarge,
            format!(
                "video exceeds the {} MiB upload limit",
                limits.upload_bytes / 1024 / 1024
            ),
        ));
    }
    let sniff_len = video_bytes.len().min(VIDEO_SNIFF_BYTES);
    if !container_matches(container, &video_bytes[..sniff_len]) {
        return Err(VideoExtractError::new(
            VideoErrorKind::UnsupportedType,
            format!(
                "file content does not match a {container} container \
                 (extension does not match supported media content)"
            ),
        ));
    }
    let size_bytes = video_bytes.len();
    let work_dir = WorkDir::new().map_err(|_| {
        VideoExtractError::new(
            VideoErrorKind::Internal,
            "could not create a temporary work directory",
        )
    })?;
    // The staged name is derived from the VALIDATED container, never from
    // user input, so no user-controlled path segment exists on disk.
    let video_path = work_dir.path.join(format!("upload.{container}"));
    std::fs::write(&video_path, &video_bytes).map_err(|_| {
        VideoExtractError::new(
            VideoErrorKind::Internal,
            "could not stage the uploaded video",
        )
    })?;
    drop(video_bytes);

    let probe = probe_video(program, limits, work_dir.path(), &video_path)?;
    if probe.duration_seconds <= 0.0 {
        return Err(VideoExtractError::new(
            VideoErrorKind::InvalidMedia,
            "video has no readable duration",
        ));
    }
    if probe.duration_seconds > limits.duration_seconds {
        return Err(VideoExtractError::new(
            VideoErrorKind::DurationTooLong,
            format!(
                "video duration {:.1}s exceeds the {}s limit",
                probe.duration_seconds, limits.duration_seconds as u64
            ),
        ));
    }
    let frame_paths = extract_frames(
        program,
        limits,
        work_dir.path(),
        &video_path,
        probe.duration_seconds,
    )?;

    let mut frames = Vec::with_capacity(frame_paths.len());
    let mut total_raw = 0usize;
    for (index, path) in frame_paths.iter().enumerate() {
        let data = std::fs::read(path).map_err(|_| {
            VideoExtractError::new(
                VideoErrorKind::Internal,
                "could not read an extracted frame",
            )
        })?;
        if data.len() > limits.frame_jpeg_bytes {
            return Err(VideoExtractError::new(
                VideoErrorKind::InvalidMedia,
                format!(
                    "extracted frame {} exceeds the {} KiB JPEG cap",
                    index + 1,
                    limits.frame_jpeg_bytes / 1024
                ),
            ));
        }
        total_raw = total_raw.checked_add(data.len()).ok_or_else(|| {
            VideoExtractError::new(VideoErrorKind::Internal, "frame byte total overflowed")
        })?;
        let Some((width, height)) = jpeg_dimensions(&data) else {
            return Err(VideoExtractError::new(
                VideoErrorKind::Internal,
                "extracted frame is not a valid JPEG",
            ));
        };
        let timestamp_seconds =
            index as f64 * probe.duration_seconds / limits.max_frames as f64;
        frames.push(VideoFrame {
            index,
            timestamp_seconds: (timestamp_seconds * 100.0).round() / 100.0,
            mime_type: "image/jpeg".to_owned(),
            width,
            height,
            size_bytes: data.len(),
            data: base64::engine::general_purpose::STANDARD.encode(&data),
        });
    }
    if total_raw > limits.frames_total_bytes {
        return Err(VideoExtractError::new(
            VideoErrorKind::InvalidMedia,
            format!(
                "extracted frames exceed the {} MiB total JPEG cap",
                limits.frames_total_bytes / 1024 / 1024
            ),
        ));
    }
    let instruction = build_instruction(&name, container, probe.duration_seconds, &frames);
    Ok(ExtractedVideo {
        name,
        container,
        mime_type,
        size_bytes,
        duration_seconds: probe.duration_seconds,
        frames,
        instruction,
    })
}

struct ProbeInfo {
    duration_seconds: f64,
    has_video_stream: bool,
}

fn probe_video(
    program: &Path,
    limits: VideoLimits,
    work_dir: &Path,
    video_path: &Path,
) -> Result<ProbeInfo, VideoExtractError> {
    let args = vec![
        "-hide_banner".to_owned(),
        "-nostdin".to_owned(),
        "-i".to_owned(),
        video_path.to_string_lossy().into_owned(),
    ];
    let output = run_ffmpeg(program, &args, work_dir, limits.probe_timeout);
    if let Some(kind) = output.spawn_error {
        return Err(ffmpeg_spawn_error(kind));
    }
    if output.timed_out {
        return Err(VideoExtractError::new(
            VideoErrorKind::Timeout,
            format!(
                "video inspection timed out after {}s",
                limits.probe_timeout.as_secs()
            ),
        ));
    }
    if output.capped {
        return Err(VideoExtractError::new(
            VideoErrorKind::InvalidMedia,
            "video inspection produced excessive output (possibly malformed media)",
        ));
    }
    // `ffmpeg -i <file>` without an output prints the input info to stderr
    // and exits 1 ("At least one output file must be specified"); that is
    // the expected probe outcome. Any other exit is a genuine read failure.
    if !matches!(output.status, Some(0) | Some(1)) {
        return Err(VideoExtractError::new(
            VideoErrorKind::InvalidMedia,
            format!("not a decodable video: {}", detail(&output.stderr)),
        ));
    }
    let Some(info) = parse_probe_output(&output.stderr) else {
        return Err(VideoExtractError::new(
            VideoErrorKind::InvalidMedia,
            "not a decodable video: no readable video stream or duration",
        ));
    };
    if !info.has_video_stream {
        return Err(VideoExtractError::new(
            VideoErrorKind::InvalidMedia,
            "not a decodable video: no video stream found",
        ));
    }
    Ok(info)
}

fn extract_frames(
    program: &Path,
    limits: VideoLimits,
    work_dir: &Path,
    video_path: &Path,
    duration_seconds: f64,
) -> Result<Vec<PathBuf>, VideoExtractError> {
    let fps = limits.max_frames as f64 / duration_seconds;
    let output_pattern = work_dir.join("frame_%02d.jpg");
    let args = vec![
        "-hide_banner".to_owned(),
        "-nostdin".to_owned(),
        "-y".to_owned(),
        "-i".to_owned(),
        video_path.to_string_lossy().into_owned(),
        "-vf".to_owned(),
        format!(
            "scale=min({FRAME_SCALE_WIDTH}\\,iw):-2,fps={fps:.6}"
        ),
        "-frames:v".to_owned(),
        limits.max_frames.to_string(),
        "-q:v".to_owned(),
        FRAME_JPEG_QUALITY.to_string(),
        output_pattern.to_string_lossy().into_owned(),
    ];
    let output = run_ffmpeg(program, &args, work_dir, limits.extract_timeout);
    if let Some(kind) = output.spawn_error {
        return Err(ffmpeg_spawn_error(kind));
    }
    if output.timed_out {
        return Err(VideoExtractError::new(
            VideoErrorKind::Timeout,
            format!(
                "frame extraction timed out after {}s",
                limits.extract_timeout.as_secs()
            ),
        ));
    }
    if output.capped {
        return Err(VideoExtractError::new(
            VideoErrorKind::InvalidMedia,
            "frame extraction produced excessive output (possibly malformed media)",
        ));
    }
    if output.status != Some(0) {
        return Err(VideoExtractError::new(
            VideoErrorKind::InvalidMedia,
            format!("frame extraction failed: {}", detail(&output.stderr)),
        ));
    }
    let mut frames = Vec::new();
    for index in 1..=limits.max_frames {
        let path = work_dir.join(format!("frame_{index:02}.jpg"));
        if path.is_file() {
            frames.push(path);
        } else {
            break;
        }
    }
    if frames.is_empty() {
        return Err(VideoExtractError::new(
            VideoErrorKind::InvalidMedia,
            "video contains no decodable video frames",
        ));
    }
    Ok(frames)
}

fn ffmpeg_spawn_error(kind: io::ErrorKind) -> VideoExtractError {
    if kind == io::ErrorKind::NotFound {
        VideoExtractError::new(
            VideoErrorKind::FfmpegMissing,
            "video preprocessing requires ffmpeg (ffprobe is not needed): install it \
             (e.g. `apt install ffmpeg`, `brew install ffmpeg`, or `choco install ffmpeg`) \
             or make sure it is on PATH",
        )
    } else {
        VideoExtractError::new(
            VideoErrorKind::Internal,
            format!("failed to start ffmpeg ({kind:?})"),
        )
    }
}

/// Terminate the child's WHOLE process tree. On unix the child was spawned
/// in its own process group, so killing the group reaps every descendant
/// (e.g. a shell wrapper's `sleep`) instead of leaving one holding the
/// stderr pipe; the direct-child kill + wait is retained as the portable
/// fallback and reap.
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Last non-empty stderr line, bounded — the most actionable fragment of
/// ffmpeg's diagnostics.
fn detail(stderr: &str) -> String {
    let last = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut chars = last.chars();
    let bounded: String = chars.by_ref().take(160).collect();
    if chars.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

struct FfmpegOutput {
    spawn_error: Option<io::ErrorKind>,
    timed_out: bool,
    capped: bool,
    status: Option<i32>,
    stderr: String,
}

/// Spawn ffmpeg directly (no shell) with a bounded stderr pipe and a
/// wall-clock deadline. The stderr reader runs on a helper thread so the
/// caller cannot be stalled by a child that floods output: once the cap is
/// hit the child is killed. Diagnostics are path-scrubbed and
/// secret-redacted before they can reach any error surface.
fn run_ffmpeg(program: &Path, args: &[String], work_dir: &Path, timeout: Duration) -> FfmpegOutput {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // The child runs in its own process group (unix) so a deadline kill can
    // terminate the WHOLE tree; a shell wrapper's own `sleep`/child would
    // otherwise keep the stderr pipe open past the timeout.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return FfmpegOutput {
                spawn_error: Some(error.kind()),
                timed_out: false,
                capped: false,
                status: None,
                stderr: String::new(),
            };
        }
    };
    let mut stderr_pipe = child.stderr.take().expect("ffmpeg stderr pipe");
    let capped = std::sync::Arc::new(AtomicBool::new(false));
    let reader_capped = std::sync::Arc::clone(&capped);
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            match stderr_pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    if buffer.len() < MAX_FFMPEG_STDERR_BYTES {
                        let room = MAX_FFMPEG_STDERR_BYTES - buffer.len();
                        buffer.extend_from_slice(&chunk[..count.min(room)]);
                        if count > room {
                            reader_capped.store(true, Ordering::Relaxed);
                            break;
                        }
                    } else {
                        reader_capped.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = stderr_tx.send(buffer);
    });

    let deadline = std::time::Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if std::time::Instant::now() >= deadline {
            timed_out = true;
            kill_process_tree(&mut child);
            break None;
        }
        if capped.load(Ordering::Relaxed) {
            kill_process_tree(&mut child);
            break None;
        }
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                kill_process_tree(&mut child);
                break None;
            }
        }
    };
    // Bounded handoff from the reader: after the (group) kill the pipe
    // closes and the reader finishes promptly. A bounded grace covers a
    // platform where a rogue descendant still holds the pipe; beyond it the
    // reader thread is abandoned rather than blocking the request forever.
    let stderr_bytes = match stderr_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(bytes) => bytes,
        Err(_) => Vec::new(),
    };
    drop(reader);
    let mut stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    let work_dir_str = work_dir.to_string_lossy();
    stderr = stderr.replace(work_dir_str.as_ref(), "<workdir>");
    let backslashed = work_dir_str.replace('/', "\\");
    if backslashed != work_dir_str.as_ref() {
        stderr = stderr.replace(&backslashed, "<workdir>");
    }
    let stderr = pi_coding::redact::redact_bounded(&stderr, MAX_FFMPEG_STDERR_BYTES);
    FfmpegOutput {
        spawn_error: None,
        timed_out,
        capped: capped.load(Ordering::Relaxed),
        status,
        stderr,
    }
}

/// Parse the ffmpeg `-i` input block: the `Duration:` line and the presence
/// of a `Video:` stream. Only lines inside the `Input #` block count, so
/// stray diagnostic lines elsewhere cannot influence the result.
fn parse_probe_output(stderr: &str) -> Option<ProbeInfo> {
    let block_start = stderr
        .lines()
        .position(|line| line.trim_start().starts_with("Input #"))?;
    let block = stderr.lines().skip(block_start).collect::<Vec<_>>().join("\n");
    let has_video_stream = block.lines().any(|line| line.contains(": Video:"));
    let duration_seconds = parse_ffmpeg_duration(&block)?;
    Some(ProbeInfo {
        duration_seconds,
        has_video_stream,
    })
}

/// Parse `Duration: HH:MM:SS.ffffff` (or `SS.ffffff`) from the ffmpeg
/// input block. Rejects negative components, non-finite values, and
/// out-of-range minutes/seconds (>= 60) so a maliciously crafted or corrupt
/// probe line can never claim an absurd duration.
///
/// The match is anchored to the OFFICIAL block-level line, which ffmpeg
/// always prints with a two-space indent (`  Duration: ...`). User-controlled
/// metadata entries (printed deeper-indented under `Metadata:`) that happen
/// to contain `Duration:` text can never preempt it.
fn parse_ffmpeg_duration(stderr: &str) -> Option<f64> {
    let line = stderr.lines().find(|line| {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("Duration:") {
            return false;
        }
        // Official line: two leading spaces (or none on unusual builds).
        // Metadata entries are indented four or more spaces.
        line.len() - trimmed.len() <= 2
    })?;
    let rest = line.split("Duration:").nth(1)?.trim_start();
    let time = rest.split(',').next()?.trim();
    if time == "N/A" {
        return None;
    }
    let mut parts = time.splitn(3, ':');
    let hours: f64 = parts.next()?.trim().parse().ok()?;
    let minutes: f64 = parts.next()?.trim().parse().ok()?;
    let seconds: f64 = parts.next()?.trim().parse().ok()?;
    if !hours.is_finite() || !minutes.is_finite() || !seconds.is_finite()
        || hours < 0.0
        || minutes < 0.0
        || seconds < 0.0
        || minutes >= 60.0
        || seconds >= 60.0
    {
        return None;
    }
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

/// Request-scoped random work directory under the system temp root, removed
/// on drop. The name is generated (never user-derived), so there is no
/// symlink or traversal surface, and the path is scrubbed from any ffmpeg
/// diagnostics before they can leak.
struct WorkDir {
    path: PathBuf,
}

impl WorkDir {
    fn new() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!("pi-video-{}", Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        }
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Test fixture shared with `file_args`' TUI `@video` expansion tests: a
/// fake `ffmpeg` executable plus directive-carrying "video" bytes.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A real 1x1 baseline JPEG (125 bytes) embedded in the fake ffmpeg so
    /// extraction produces structurally valid frames.
    pub(crate) const FAKE_JPEG_B64: &str = "/9j/4AAQSkZJRgABAQEAYABgAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/wAALCAABAAEBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAACf/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AVN//2Q==";

    /// Fake `ffmpeg` (POSIX sh) for pi-cli video tests. The uploaded
    /// "video" file carries a directive on its first line (magic bytes may
    /// precede it). Probe invocations (args end without a `frame_%02d.jpg`
    /// output) print an ffmpeg-style input block and exit 1; extraction
    /// invocations write `-frames:v` JPEG files from the embedded constant.
    pub(crate) const FAKE_SCRIPT: &str = r#"#!/bin/sh
input=""
last=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "-i" ]; then input="$arg"; fi
  prev="$arg"
  last="$arg"
done
if [ -z "$input" ]; then
  echo "fake-ffmpeg: missing -i input" >&2
  exit 2
fi
directive=$(grep -a -m1 -E 'VALID |MISSING_DURATION|NO_VIDEO|CORRUPT|HUGE_STDERR|SLOW_PROBE|SLOW_EXTRACT|EXTRACT_FAIL' "$input" 2>/dev/null)
mode=$(printf '%s' "$directive" | grep -a -m1 -o -E 'VALID |MISSING_DURATION|NO_VIDEO|CORRUPT|HUGE_STDERR|SLOW_PROBE|SLOW_EXTRACT|EXTRACT_FAIL' | tr -d ' ')

is_extract=0
case "$last" in
  *frame_%02d.jpg) is_extract=1 ;;
esac

if [ "$is_extract" = "1" ]; then
  case "$mode" in
    VALID)
      n=6
      prev=""
      for arg in "$@"; do
        if [ "$prev" = "-frames:v" ]; then n="$arg"; fi
        prev="$arg"
      done
      dir=$(dirname "$last")
      i=1
      while [ "$i" -le "$n" ]; do
        f=$(printf '%s/frame_%02d.jpg' "$dir" "$i")
        printf '%s' '__FAKE_JPEG__' | base64 -d > "$f"
        i=$((i+1))
      done
      echo "frame= $n fps=0.0 q=3.0" >&2
      exit 0
      ;;
    EXTRACT_FAIL)
      echo "Error while decoding stream #0:0: Invalid data found" >&2
      exit 1
      ;;
    SLOW_EXTRACT)
      sleep 5
      exit 0
      ;;
    *)
      echo "fake-ffmpeg: unknown extract directive $mode" >&2
      exit 2
      ;;
  esac
fi

case "$mode" in
  VALID|EXTRACT_FAIL|SLOW_EXTRACT)
    dur=$(printf '%s' "$directive" | cut -s -d' ' -f2)
    dims=$(printf '%s' "$directive" | cut -s -d' ' -f3)
    [ -z "$dur" ] && dur="00:00:05.00"
    [ -z "$dims" ] && dims="640x360"
    echo "Input #0, matroska,webm, from '$input':" >&2
    echo "  Metadata:" >&2
    echo "    title           : fake" >&2
    echo "  Duration: $dur, start: 0.000000, bitrate: 1000 kb/s" >&2
    echo "  Stream #0:0: Video: h264 (High), yuv420p(progressive), $dims [SAR 1:1 DAR 16:9], 30 fps, 30 tbr, 90k tbn (default)" >&2
    echo "  Stream #0:1: Audio: aac (LC), 48000 Hz, stereo, fltp, 128 kb/s (default)" >&2
    echo "At least one output file must be specified" >&2
    exit 1
    ;;
  MISSING_DURATION)
    echo "Input #0, matroska,webm, from '$input':" >&2
    echo "  Stream #0:0: Video: h264 (High), yuv420p(progressive), 1280x720 [SAR 1:1 DAR 16:9], 30 fps, 30 tbr, 90k tbn (default)" >&2
    exit 1
    ;;
  NO_VIDEO)
    echo "Input #0, matroska,webm, from '$input':" >&2
    echo "  Duration: 00:00:05.00, start: 0.000000, bitrate: 320 kb/s" >&2
    echo "  Stream #0:0: Audio: aac (LC), 48000 Hz, stereo, fltp, 128 kb/s (default)" >&2
    exit 1
    ;;
  CORRUPT)
    echo "Invalid data found when processing input" >&2
    exit 2
    ;;
  HUGE_STDERR)
    i=0
    while [ "$i" -lt 2000 ]; do
      echo "XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"
      i=$((i+1))
    done
    exit 1
    ;;
  SLOW_PROBE)
    sleep 5
    exit 1
    ;;
  *)
    echo "Unknown input format for '$input'" >&2
    exit 2
    ;;
esac
"#;

    /// Write the fake ffmpeg script (chmod +x) into a fresh directory.
    pub(crate) fn fake_ffmpeg() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("fake ffmpeg dir");
        let script = dir.path().join("fake-ffmpeg.sh");
        std::fs::write(&script, FAKE_SCRIPT.replace("__FAKE_JPEG__", FAKE_JPEG_B64))
            .expect("write fake ffmpeg");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake ffmpeg");
        }
        (dir, script)
    }

    /// Uploaded "video" bytes: EBML magic + padding, then the directive on
    /// its OWN line, then padding. The container check (magic within the
    /// first 64 bytes) and the fake's line-based directive grep both work,
    /// and the directive line stays clean ASCII so shell field parsing of
    /// `dur`/`dims` cannot be corrupted by the binary magic prefix.
    pub(crate) fn video_bytes(directive: &str) -> Vec<u8> {
        let mut bytes = vec![0x1A, 0x45, 0xDF, 0xA3];
        bytes.extend(std::iter::repeat_n(b'x', 48));
        bytes.push(b'\n');
        bytes.extend_from_slice(directive.as_bytes());
        bytes.push(b'\n');
        bytes.extend(std::iter::repeat_n(b'x', 150));
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    use super::test_support::{FAKE_JPEG_B64, fake_ffmpeg, video_bytes};

    fn extract(
        script: &Path,
        limits: VideoLimits,
        bytes: Vec<u8>,
        name: &str,
    ) -> Result<ExtractedVideo, VideoExtractError> {
        // Serialize fake-ffmpeg tests: at most one fixture subprocess runs
        // at a time across the suite, so no cross-test process/resource
        // contention can flake the deterministic fixture tests.
        let _serial = FFMPEG_TEST_GUARD.lock().expect("serialize video tests");
        extract_video(script, limits, bytes, name)
    }

    #[test]
    fn sanitize_accepts_all_containers_and_normalizes_names() {
        for (container, mime) in VIDEO_CONTAINERS {
            let (name, got_container, got_mime) =
                sanitize_video_name(&format!("My Clip.{container}")).expect("allowed container");
            assert_eq!(name, format!("My Clip.{container}"));
            assert_eq!(got_container, *container);
            assert_eq!(got_mime, *mime);
        }
        // Case-insensitive extension, lowercased in the output name.
        let (name, container, _) = sanitize_video_name("clip.MKV").expect("case-insensitive");
        assert_eq!(name, "clip.mkv");
        assert_eq!(container, "mkv");
        // Path separators and controls collapse to spaces (display only).
        let (name, _, _) = sanitize_video_name("a/b\\c\u{1f}.mkv").expect("sanitized");
        assert!(!name.contains(['\\', '\u{1f}']));
        // Absolute paths collapse to the basename: never expose locations.
        let (name, _, _) = sanitize_video_name("<workspace>/secret/clip.mkv").expect("basename");
        assert_eq!(name, "clip.mkv");
        // Relative paths collapse to the basename too: no directory
        // structure may project into the marker.
        let (name, _, _) = sanitize_video_name("dir/private/clip.mkv").expect("relative basename");
        assert_eq!(name, "clip.mkv");
        let (name, _, _) = sanitize_video_name("dir\\private\\clip.mkv").expect("windows basename");
        assert_eq!(name, "clip.mkv");
        // Whatever the input, the sanitized name never contains a separator.
        for input in [
            "clip.mkv",
            "/a/b/clip.mkv",
            "a/b/clip.mkv",
            "a\\b\\clip.mkv",
            "..\\..\\etc\\passwd.mkv",
            "a b/c-d_e.f.mkv",
        ] {
            let (name, _, _) = sanitize_video_name(input).expect("sanitizable");
            assert!(
                !name.contains(['/', '\\']),
                "sanitized name must never contain a separator: {name}"
            );
        }
        // Empty stem gets a fallback; long stems are bounded.
        let (name, _, _) = sanitize_video_name(".mkv").expect("empty stem");
        assert_eq!(name, "video.mkv");
        let long = format!("{}.mkv", "a".repeat(500));
        let (name, _, _) = sanitize_video_name(&long).expect("long stem");
        assert!(name.len() <= MAX_VIDEO_NAME_CHARS + 4);
    }

    #[test]
    fn sanitize_rejects_non_video_extensions() {
        for name in ["clip.txt", "clip.mp3", "clip", "no-extension", "", "a" ] {
            assert!(sanitize_video_name(name).is_none(), "{name:?} must be rejected");
        }
    }

    #[test]
    fn container_magic_bytes_match_and_mismatch() {
        let mkv = [0x1A, 0x45, 0xDF, 0xA3, 0x00, 0x00];
        assert!(container_matches("mkv", &mkv));
        assert!(container_matches("webm", &mkv));
        let mp4 = b"\x00\x00\x00\x18ftypisom";
        assert!(container_matches("mp4", mp4));
        assert!(container_matches("mov", b"\x00\x00\x00\x14ftypqt  "));
        let avi = b"RIFF\x24\x00\x00\x00AVI LIST";
        assert!(container_matches("avi", avi));
        assert!(container_matches("ogg", b"OggS\x00\x02"));
        // Mismatches: right extension, wrong bytes.
        assert!(!container_matches("mkv", b"RIFF....AVI "));
        assert!(!container_matches("mp4", &mkv));
        assert!(!container_matches("avi", mp4));
        assert!(!container_matches("ogg", &mkv));
        assert!(!container_matches("unknown", &mkv));
    }

    #[test]
    fn jpeg_dimensions_parse_sofs_and_reject_garbage() {
        let one_px = base64::engine::general_purpose::STANDARD
            .decode(FAKE_JPEG_B64)
            .expect("embedded JPEG decodes");
        assert_eq!(jpeg_dimensions(&one_px), Some((1, 1)));
        assert_eq!(jpeg_dimensions(b"not a jpeg"), None);
        assert_eq!(jpeg_dimensions(&one_px[..20]), None);
        assert_eq!(jpeg_dimensions(&[0xFF, 0xD8, 0xFF, 0xDA]), None); // SOS before SOF
        let mut truncated = one_px.clone();
        truncated.truncate(40);
        assert_eq!(jpeg_dimensions(&truncated), None);
    }

    #[test]
    fn probe_parsing_extracts_duration_and_video_stream() {
        let sample = concat!(
            "Input #0, matroska,webm, from '<workspace>/upload.mkv':\n",
            "  Duration: 00:00:12.34, start: 0.000000, bitrate: 1000 kb/s\n",
            "  Stream #0:0: Video: h264 (High), yuv420p(progressive), 1280x720 [SAR 1:1 DAR 16:9], 30 fps, 30 tbr, 90k tbn (default)\n",
            "At least one output file must be specified\n",
        );
        let info = parse_probe_output(sample).expect("probe parses");
        assert!((info.duration_seconds - 12.34).abs() < 1e-9);
        assert!(info.has_video_stream);

        // Audio-only: duration parses, no video stream.
        let audio = concat!(
            "Input #0, ogg, from 'x.ogg':\n",
            "  Duration: 00:00:05.00, start: 0.000000, bitrate: 320 kb/s\n",
            "  Stream #0:0: Audio: vorbis, 44100 Hz, stereo, fltp (default)\n",
        );
        let info = parse_probe_output(audio).expect("audio probe parses");
        assert!(!info.has_video_stream);

        // No Duration line at all.
        assert!(parse_probe_output("Input #0, matroska, from 'x':\n").is_none());
        // Duration: N/A
        assert!(parse_probe_output("Duration: N/A, start: 0.000000\n").is_none());
        // Malicious or corrupt out-of-range components are rejected.
        assert!(parse_probe_output("Duration: 00:00:61.00, start: 0.000000\n").is_none());
        assert!(parse_probe_output("Duration: 00:61:00.00, start: 0.000000\n").is_none());
        assert!(parse_probe_output("Duration: 00:00:-1.00, start: 0.000000\n").is_none());
        assert!(parse_probe_output("Duration: 1e999:00:00.00, start: 0.000000\n").is_none());
        assert!(parse_probe_output("Duration: -01:00:00.00, start: 0.000000\n").is_none());
        // User-controlled Metadata entries containing `Duration:` text can
        // never preempt the official two-space-indented block line.
        let metadata_smuggled = concat!(
            "Input #0, matroska,webm, from 'x.mkv':\n",
            "  Metadata:\n",
            "    Duration: 999999.00, start: 0.000000, bitrate: 999 kb/s\n",
            "    title           : Duration: fake\n",
            "  Duration: 00:00:12.34, start: 0.000000, bitrate: 1000 kb/s\n",
            "  Stream #0:0: Video: h264 (High), yuv420p(progressive), 640x360 [SAR 1:1 DAR 16:9], 30 fps, 30 tbr, 90k tbn (default)\n",
        );
        let info = parse_probe_output(metadata_smuggled).expect("official line wins");
        assert!((info.duration_seconds - 12.34).abs() < 1e-9);
        // Metadata-only (no official line) is not a duration.
        let metadata_only = "Input #0, matroska, from 'x.mkv':\n  Metadata:\n    Duration: 5.00\n";
        assert!(parse_probe_output(metadata_only).is_none());
    }

    #[test]
    fn instruction_is_bounded_and_chronological() {
        let frames = vec![
            VideoFrame {
                index: 0,
                timestamp_seconds: 0.0,
                mime_type: "image/jpeg".into(),
                width: 640,
                height: 360,
                size_bytes: 100,
                data: "AA==".into(),
            },
            VideoFrame {
                index: 1,
                timestamp_seconds: 2.06,
                mime_type: "image/jpeg".into(),
                width: 640,
                height: 360,
                size_bytes: 100,
                data: "AA==".into(),
            },
        ];
        let text = build_instruction("clip.mkv", "mkv", 12.34, &frames);
        assert!(text.contains("clip.mkv"));
        assert!(text.contains("0.00s"));
        assert!(text.contains("2.06s"));
        assert!(text.len() <= MAX_VIDEO_INSTRUCTION_CHARS);
        assert!(text.find("0.00s").unwrap() < text.find("2.06s").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn work_dir_is_removed_on_drop() {
        let dir = WorkDir::new().expect("work dir");
        let path = dir.path.clone();
        assert!(path.is_dir());
        drop(dir);
        assert!(!path.exists(), "work dir must be cleaned on drop");
    }

    #[cfg(unix)]
    #[test]
    fn valid_video_extracts_chronological_jpeg_frames() {
        let (_dir, script) = fake_ffmpeg();
        let video = extract(
            &script,
            VideoLimits::default(),
            video_bytes("VALID 00:00:12.34 1280x720"),
            "clip.mkv",
        )
        .expect("valid mkv extracts");
        assert_eq!(video.name, "clip.mkv");
        assert_eq!(video.container, "mkv");
        assert_eq!(video.mime_type, "video/x-matroska");
        let expected_size = video_bytes("VALID 00:00:12.34 1280x720").len();
        assert_eq!(video.size_bytes, expected_size);
        assert_eq!(video.frames.len(), MAX_VIDEO_FRAMES);
        assert_eq!(video.frames_base64_bytes() % 4, 0);
        for (index, frame) in video.frames.iter().enumerate() {
            assert_eq!(frame.index, index);
            assert_eq!(frame.mime_type, "image/jpeg");
            assert_eq!((frame.width, frame.height), (1, 1));
            assert!(frame.size_bytes > 0);
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(&frame.data)
                    .expect("frame base64 decodes")
                    .len(),
                frame.size_bytes
            );
            if index > 0 {
                assert!(
                    frame.timestamp_seconds > video.frames[index - 1].timestamp_seconds,
                    "frames must be chronological"
                );
            }
        }
        assert_eq!(video.frames[0].timestamp_seconds, 0.0);
        let step = 12.34 / MAX_VIDEO_FRAMES as f64;
        for (index, frame) in video.frames.iter().enumerate() {
            let expected = ((index as f64 * step) * 100.0).round() / 100.0;
            assert!(
                (frame.timestamp_seconds - expected).abs() < 1e-9,
                "frame {index} timestamp {} != {expected}",
                frame.timestamp_seconds
            );
        }
        assert!(video.instruction.contains("clip.mkv"));
        assert!(video.instruction.contains("12.34"));
        assert!(video.instruction.contains("2.06s"));
        assert!(video.instruction.contains("6 chronological JPEG frames"));
        assert!(
            !video.instruction.contains("pi-video-"),
            "instruction must not leak the work dir path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn corrupt_and_streamless_inputs_fail_before_prompting() {
        let (_dir, script) = fake_ffmpeg();
        let limits = VideoLimits::default();

        let error = extract(&script, limits, video_bytes("CORRUPT"), "clip.mkv")
            .expect_err("corrupt media must fail");
        assert_eq!(error.kind, VideoErrorKind::InvalidMedia);
        assert!(error.message.contains("not a decodable video"));
        assert!(!error.message.contains("pi-video-"), "no path leak: {}", error.message);

        let error = extract(&script, limits, video_bytes("MISSING_DURATION"), "clip.mkv")
            .expect_err("missing duration must fail");
        assert_eq!(error.kind, VideoErrorKind::InvalidMedia);

        let error = extract(&script, limits, video_bytes("NO_VIDEO"), "clip.mkv")
            .expect_err("audio-only must fail");
        assert_eq!(error.kind, VideoErrorKind::InvalidMedia);
        assert!(error.message.contains("no video stream"));

        let error = extract(&script, limits, video_bytes("EXTRACT_FAIL"), "clip.mkv")
            .expect_err("extraction failure must fail");
        assert_eq!(error.kind, VideoErrorKind::InvalidMedia);
        assert!(error.message.contains("frame extraction failed"));
    }

    #[cfg(unix)]
    #[test]
    fn name_and_container_rejections_are_typed() {
        let (_dir, script) = fake_ffmpeg();
        let limits = VideoLimits::default();

        let error = extract(
            &script,
            limits,
            video_bytes("VALID 00:00:01.00 320x240"),
            "dir/secret/clip.txt",
        )
        .expect_err("non-video extension must fail");
        assert_eq!(error.kind, VideoErrorKind::UnsupportedType);
        assert!(
            !error.message.contains("secret") && !error.message.contains("clip.txt"),
            "raw name must not be echoed: {}",
            error.message
        );

        // Right extension, wrong magic (text file named .mkv).
        let error = extract(&script, limits, b"definitely not a video".to_vec(), "clip.mkv")
            .expect_err("content mismatch must fail");
        assert_eq!(error.kind, VideoErrorKind::UnsupportedType);
        assert!(error.message.contains("does not match a mkv container"));
    }

    #[cfg(unix)]
    #[test]
    fn bounds_are_enforced() {
        let (_dir, script) = fake_ffmpeg();

        // Upload byte cap (defense in depth inside the pipeline).
        let limits = VideoLimits {
            upload_bytes: 64,
            ..VideoLimits::default()
        };
        let error = extract(&script, limits, video_bytes("VALID 00:00:01.00 320x240"), "clip.mkv")
            .expect_err("oversize must fail");
        assert_eq!(error.kind, VideoErrorKind::TooLarge);

        // Empty upload.
        let error = extract(&script, VideoLimits::default(), Vec::new(), "clip.mkv")
            .expect_err("empty must fail");
        assert_eq!(error.kind, VideoErrorKind::InvalidMedia);

        // Duration cap.
        let limits = VideoLimits::default();
        let error = extract(
            &script,
            limits,
            video_bytes("VALID 00:11:40.00 1280x720"),
            "clip.mkv",
        )
        .expect_err("long duration must fail");
        assert_eq!(error.kind, VideoErrorKind::DurationTooLong);
        assert!(error.message.contains("700.0s exceeds the 600s limit"));

        // Per-frame JPEG cap.
        let limits = VideoLimits {
            frame_jpeg_bytes: 10,
            ..VideoLimits::default()
        };
        let error = extract(&script, limits, video_bytes("VALID 00:00:01.00 320x240"), "clip.mkv")
            .expect_err("large frame must fail");
        assert_eq!(error.kind, VideoErrorKind::InvalidMedia);
        assert!(error.message.contains("JPEG cap"));

        // Total frames cap.
        let limits = VideoLimits {
            frames_total_bytes: 10,
            ..VideoLimits::default()
        };
        let error = extract(&script, limits, video_bytes("VALID 00:00:01.00 320x240"), "clip.mkv")
            .expect_err("large total must fail");
        assert_eq!(error.kind, VideoErrorKind::InvalidMedia);

        // Frame count cap (fake honors -frames:v).
        let limits = VideoLimits {
            max_frames: 2,
            ..VideoLimits::default()
        };
        let video = extract(&script, limits, video_bytes("VALID 00:00:10.00 1280x720"), "clip.mkv")
            .expect("two frames extract");
        assert_eq!(video.frames.len(), 2);
        assert_eq!(video.frames[0].timestamp_seconds, 0.0);
        assert!((video.frames[1].timestamp_seconds - 5.0).abs() < 1e-9);
    }

    #[cfg(unix)]
    #[test]
    fn probe_and_extract_timeouts_are_bounded() {
        let (_dir, script) = fake_ffmpeg();

        // Each timeout must fire at the configured limit — NOT after waiting
        // out a descendant that holds the stderr pipe (the fixture's
        // `sleep 5` proves the process group is terminated).
        let limits = VideoLimits {
            probe_timeout: Duration::from_millis(100),
            ..VideoLimits::default()
        };
        let start = std::time::Instant::now();
        let error = extract(&script, limits, video_bytes("SLOW_PROBE"), "clip.mkv")
            .expect_err("slow probe must time out");
        assert_eq!(error.kind, VideoErrorKind::Timeout);
        assert!(error.message.contains("timed out"));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "probe timeout waited {}ms instead of ~100ms",
            start.elapsed().as_millis()
        );

        let limits = VideoLimits {
            extract_timeout: Duration::from_millis(100),
            ..VideoLimits::default()
        };
        let start = std::time::Instant::now();
        let error = extract(&script, limits, video_bytes("SLOW_EXTRACT"), "clip.mkv")
            .expect_err("slow extract must time out");
        assert_eq!(error.kind, VideoErrorKind::Timeout);
        assert!(error.message.contains("timed out"));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "extract timeout waited {}ms instead of ~100ms",
            start.elapsed().as_millis()
        );
    }

    #[cfg(unix)]
    #[test]
    fn excessive_stderr_is_capped_and_kills_the_child() {
        let (_dir, script) = fake_ffmpeg();
        let error = extract(
            &script,
            VideoLimits::default(),
            video_bytes("HUGE_STDERR"),
            "clip.mkv",
        )
        .expect_err("pathological output must fail");
        assert_eq!(error.kind, VideoErrorKind::InvalidMedia);
        assert!(error.message.len() < 400, "error must stay bounded");
    }

    #[cfg(unix)]
    #[test]
    fn missing_ffmpeg_is_actionable() {
        let (_dir, _script) = fake_ffmpeg();
        let missing = _dir.path().join("does-not-exist");
        let error = extract(&missing, VideoLimits::default(), video_bytes("VALID 00:00:01.00 320x240"), "clip.mkv")
            .expect_err("missing ffmpeg must fail");
        assert_eq!(error.kind, VideoErrorKind::FfmpegMissing);
        assert!(error.message.contains("install"));
        assert!(error.message.contains("ffmpeg"));
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn error_status_mapping_matches_the_wire_contract() {
        let cases = [
            (VideoErrorKind::FfmpegMissing, StatusCode::SERVICE_UNAVAILABLE),
            (VideoErrorKind::UnsupportedType, StatusCode::UNSUPPORTED_MEDIA_TYPE),
            (VideoErrorKind::TooLarge, StatusCode::PAYLOAD_TOO_LARGE),
            (VideoErrorKind::DurationTooLong, StatusCode::UNPROCESSABLE_ENTITY),
            (VideoErrorKind::Timeout, StatusCode::GATEWAY_TIMEOUT),
            (VideoErrorKind::InvalidMedia, StatusCode::BAD_REQUEST),
        ];
        for (kind, status) in cases {
            let error = VideoExtractError::new(kind, "test");
            assert_eq!(error.status(), status);
        }
    }
}
