//! Minimal hold-to-talk realtime voice (`/live`): microphone capture →
//! speech-to-text → composer draft.
//!
//! Modeled on Hyper's Codex Live, but deliberately simple:
//!
//! - **Ptt state machine** — press starts recording, release transcribes,
//!   a new press supersedes the previous session, a 10-second no-speech
//!   watchdog stops capture with guidance, abort drops the capture.
//! - **Bounded audio backlog** — a held utterance is capped at
//!   [`MAX_RECORDING_SECONDS`] of mono 16 kHz i16 PCM; oldest samples drop
//!   first under pathological holds.
//! - **User-configured STT** — the endpoint base URL and API key come
//!   entirely from `Settings.live`; this module never assumes (or contacts)
//!   OpenAI. The wire shape is the OpenAI-compatible
//!   `POST {base}/v1/audio/transcriptions` multipart form (`file`, `model`,
//!   optional `language`) with a `Bearer` header.
//! - **TLS-only by default** — `http://` and `ws://`/`wss://` endpoints are
//!   rejected with an actionable error unless `allowInsecure` is explicitly
//!   set; the API key is never logged or rendered (settings catalog secret
//!   marking plus error redaction).
//!
//! Follow-ups (documented, not implemented): WebRTC/Opus streaming to the
//! STT endpoint and a transcript-preview-while-speaking mode (this build
//! commits the final whole-utterance transcript to the composer draft only).
//!
//! **Delegation bridge (implemented).** Hyper's Codex Live receives
//! server-side `delegation.created` events from the voice model and submits
//! the literal text through the normal prompt path, correlating the
//! delegation with the bound agent/session until an exactly-once terminal
//! completion (analysis §7.3). rpi's STT is a generic OpenAI-compatible
//! endpoint that never emits delegation events, so the bridge detects the
//! intent client-side instead: [`is_delegation_candidate`] flags transcribed
//! text that pairs a coding verb (implement/fix/add/test/refactor/…) with a
//! code-domain signal (file path, extension, or code word). The TUI offers
//! the draft with a `⟦delegate⟧` hint and submits it through the standard
//! `Application::prompt` path — the delegation *is* an ordinary agent turn:
//! no separate task queue, and the reply merges into the transcript exactly
//! like any other turn.
//!
//! **Transcript merge.** Hyper's broker merges interim/final voice frames per
//! role and replaces a dedicated scrollback entry as cumulative resends
//! arrive (analysis §7.4). rpi's STT returns one final whole-utterance
//! transcript per release (no interim frames in this build), so the only
//! merge point is the transcript → composer-draft commit, and the agent
//! reply is appended by the normal turn machinery. There is nothing to
//! reconcile beyond what L1 already does.
//!
//! **Keepalive.** Hyper adds protocol-level keepalive pings so quiet
//! WebSocket Live sessions are not reaped by proxies or load balancers
//! (CHANGELOG:21-23). rpi's STT transport is one-shot HTTP multipart
//! request/response with a bounded 30-second client timeout — there is no
//! long-lived connection a proxy could reap, so keepalive pings are neither
//! possible nor needed.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::redact::{redact_bounded, redact_secrets};
use crate::settings::LiveRuntimeSettings;

/// Fallback model label sent as the multipart `model` field. This product
/// never resolves it against OpenAI — the user-configured base URL serves or
/// rejects it.
pub const DEFAULT_STT_MODEL: &str = "whisper-1";
/// Capture sample rate (mono i16 PCM), matching the Hyper voice pipeline.
pub const SAMPLE_RATE: u32 = 16_000;
/// Longest utterance retained per press (bounded audio backlog). 30 s of
/// mono 16 kHz i16 PCM is 960,000 bytes.
pub const MAX_RECORDING_SECONDS: f32 = 30.0;
/// The same capture bound as a `Duration`, shared by the WAV parser so a
/// header-derived duration can never exceed the retained-utterance cap.
pub const MAX_RECORDING_DURATION: Duration = Duration::from_secs(30);
/// Ten-second no-speech watchdog (Hyper parity): capture stops with guidance
/// instead of leaving a dead mic streaming indefinitely.
pub const NO_SPEECH_TIMEOUT: Duration = Duration::from_secs(10);
/// Minimum utterance length before release is worth transcribing.
pub const MIN_UTTERANCE: Duration = Duration::from_millis(250);
/// Peak sample amplitude (of 32767) treated as speech for the watchdog.
pub const SPEECH_PEAK_THRESHOLD: i16 = 300;
/// HTTP client timeout for the STT request.
pub const STT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum characters of a server body echoed into an STT error, after
/// secret redaction — a hostile or verbose endpoint can never project an
/// unbounded body into an RPC response.
pub const STT_ERROR_SNIPPET_CHARS: usize = 300;
/// Hard cap on the STT response body bytes, enforced BEFORE any unbounded
/// allocation (Content-Length declared over the cap fails immediately;
/// otherwise the body is streamed with a byte cap). A hostile endpoint can
/// never project an oversized transcript into an RPC frame or the composer.
pub const STT_MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// Cap on the extracted transcript characters (the final `text` is trimmed,
/// non-empty, and truncated to this bound for the DOM/composer).
pub const STT_MAX_TRANSCRIPT_CHARS: usize = 16 * 1024;

/// Guidance text emitted when the no-speech watchdog stops a capture.
pub const WATCHDOG_GUIDANCE: &str = "No speech detected — release and hold again to talk";

/// Multipart filename sent as `file=audio.wav` (the WAV is encoded in
/// memory; nothing touches disk).
pub const AUDIO_FILENAME: &str = "audio.wav";

// ---------------------------------------------------------------------------
// Settings validation
// ---------------------------------------------------------------------------

/// Validates that `settings` can drive a TUI `/live` hold-to-talk session.
/// Returns actionable errors — each names the exact `Settings.live.*` key to
/// fix — for the disabled, unconfigured, plaintext-endpoint, and unsupported
/// mode cases.
///
/// Mode is trimmed and compared case-insensitively. Only `"stt"` drives the
/// TUI Ptt pipeline (mic capture → STT → composer draft). `"realtime"` voice
/// runs over WebRTC, which the TUI does not implement — the user must use the
/// Web listener instead, so this validator reports that explicitly and never
/// falls through to the STT-base-url checks (which would misreport a missing
/// `sttBaseUrl` for a realtime config). Any other mode fails fast and lists
/// the legal values.
pub fn validate_live_settings(settings: &LiveRuntimeSettings) -> Result<()> {
    if !settings.enabled {
        bail!(
            "Live voice is disabled — set `Settings.live.enabled = true` (or run `/settings set live.enabled true`)"
        );
    }
    let mode = settings.mode.trim().to_ascii_lowercase();
    match mode.as_str() {
        "stt" => {}
        "realtime" => bail!(
            "TUI hold-to-talk only supports `Settings.live.mode = \"stt\"`; \
             realtime voice runs over WebRTC in the Web listener — start it with \
             `rpi --listen 127.0.0.1:8080` and open the /web page in a browser"
        ),
        other if other.is_empty() => bail!(
            "`Settings.live.mode` is empty — set it to `stt` (TUI hold-to-talk) \
             or `realtime` (Web listener)"
        ),
        other => bail!(
            "Unknown `Settings.live.mode` value `{other}` — use `stt` \
             (TUI hold-to-talk) or `realtime` (Web listener)"
        ),
    }
    if settings.stt_base_url.trim().is_empty() {
        bail!(
            "Live voice is not configured — set `Settings.live.sttBaseUrl` to your speech-to-text base URL (e.g. https://host:port) and `Settings.live.sttApiKey` to its API key"
        );
    }
    if settings.stt_api_key.trim().is_empty() {
        bail!(
            "Live voice is not configured — set `Settings.live.sttApiKey` (the API key for `Settings.live.sttBaseUrl`) in settings.json; it is secret and cannot be written through /settings"
        );
    }
    let parsed = url::Url::parse(settings.stt_base_url.trim()).context(
        "`Settings.live.sttBaseUrl` is not a valid URL — use a bare http(s)://host[:port] base URL",
    )?;
    // The configured base URL is embedded in request/error diagnostics; a
    // URL carrying credentials, a query, or a fragment could leak secret
    // material there, so those shapes are rejected outright with a fixed
    // message that never echoes the raw value.
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!(
            "`Settings.live.sttBaseUrl` must not embed credentials, a query, or a fragment — use a bare http(s)://host[:port] base URL"
        );
    }
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if settings.allow_insecure => Ok(()),
        "http" => bail!(
            "Refusing to send STT bearer credentials over plaintext: `Settings.live.sttBaseUrl` uses http:// but `Settings.live.allowInsecure` is false. Use https://, or set `Settings.live.allowInsecure = true` for a loopback/self-hosted server"
        ),
        "ws" | "wss" => bail!(
            "`Settings.live.sttBaseUrl` uses {}://, but /live speaks HTTP multipart to `{{base}}/v1/audio/transcriptions` — use an http(s) endpoint such as http://localhost:9000 or https://your-whisper-host",
            parsed.scheme()
        ),
        other => bail!(
            "Unsupported `Settings.live.sttBaseUrl` scheme `{other}://` — use https:// (or http:// with `Settings.live.allowInsecure = true`)"
        ),
    }
}

/// Builds the transcriptions URL for a configured base, de-duplicating a
/// trailing `/v1` (Hyper parity) and any trailing slashes.
pub fn transcriptions_url(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    let suffix = if base.ends_with("/v1") {
        "/audio/transcriptions"
    } else {
        "/v1/audio/transcriptions"
    };
    format!("{base}{suffix}")
}

// ---------------------------------------------------------------------------
// WAV encoding (in memory)
// ---------------------------------------------------------------------------

/// Encodes mono PCM16 samples as a RIFF/WAVE file with a PCM header.
/// `sample_rate` and `channels` describe the captured stream; the encoding is
/// always 16-bit little-endian signed samples.
pub fn encode_wav(pcm: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    let data_len = pcm.len() * 2;
    let mut wav = Vec::with_capacity(44 + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&(channels * 2).to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in pcm {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

/// Parses a RIFF/WAVE PCM16 byte buffer into an [`AudioCapture`], deriving
/// `sample_rate`, `channels`, and `duration` from the header. Strict and
/// fail-closed: rejects anything that is not a WAVE/PCM container
/// (`RIFF`/`WAVE` magic, format 1, 16-bit samples, non-zero channels/sample
/// rate/byte rate, a plausible sample rate), rejects header geometry that is
/// internally inconsistent (block align ≠ channels × 2, byte rate ≠ sample
/// rate × channels × 2, data not a whole number of blocks), requires a
/// non-empty `data` chunk whose duration stays within
/// [`MAX_RECORDING_DURATION`], and rejects ANY chunk whose declared size
/// (plus word-alignment padding) extends beyond the buffer — a truncated or
/// hostile input can never parse into an oversized duration. Chunks may
/// appear in any order (a `LIST`/`fact` chunk between `fmt ` and `data` is
/// tolerated). Used by the Web `stt_transcribe` RPC to validate
/// browser-recorded audio before it is forwarded to the STT endpoint.
pub fn parse_wav_capture(wav: &[u8]) -> Result<AudioCapture> {
    if wav.len() < 44 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        bail!("audio is not a RIFF/WAVE file");
    }
    // The RIFF header declares the file size (field = size - 8). The buffer
    // must end EXACTLY there: a shorter buffer is truncated (e.g. data cut
    // mid-chunk) and a longer one carries trailing bytes the canonical
    // browser encoder never emits — both fail closed.
    let riff_size = u32::from_le_bytes(wav[4..8].try_into().expect("4 bytes")) as usize;
    let riff_end = riff_size.saturating_add(8);
    if wav.len() < riff_end {
        bail!(
            "audio is truncated (RIFF header declares {riff_end} bytes, buffer has {})",
            wav.len()
        );
    }
    if wav.len() > riff_end {
        bail!(
            "audio has {} trailing bytes beyond the declared RIFF size ({riff_end})",
            wav.len() - riff_end
        );
    }
    let mut offset = 12usize;
    let mut fmt: Option<(u16, u16, u32, u32, u16, u16)> = None; // format, channels, sample_rate, byte_rate, block_align, bits
    let mut data_len: Option<usize> = None;
    while offset + 8 <= wav.len() {
        let id = &wav[offset..offset + 4];
        let size = u32::from_le_bytes(wav[offset + 4..offset + 8].try_into().expect("4 bytes")) as usize;
        let body = offset + 8;
        let end = body
            .checked_add(size)
            .context("audio chunk size overflows address space")?;
        if end > wav.len() {
            bail!("audio has a truncated chunk (declared {size} bytes at offset {offset})");
        }
        match id {
            b"fmt " => {
                // A second fmt chunk must not override the first (a forged
                // header could otherwise change the derived duration/rates).
                if fmt.is_some() {
                    bail!("audio has multiple fmt chunks");
                }
                if size < 16 {
                    bail!("audio has a truncated fmt chunk");
                }
                let format = u16::from_le_bytes([wav[body], wav[body + 1]]);
                let channels = u16::from_le_bytes([wav[body + 2], wav[body + 3]]);
                let sample_rate = u32::from_le_bytes(wav[body + 4..body + 8].try_into().expect("4 bytes"));
                let byte_rate = u32::from_le_bytes(wav[body + 8..body + 12].try_into().expect("4 bytes"));
                let block_align = u16::from_le_bytes([wav[body + 12], wav[body + 13]]);
                let bits = u16::from_le_bytes([wav[body + 14], wav[body + 15]]);
                fmt = Some((format, channels, sample_rate, byte_rate, block_align, bits));
            }
            b"data" => {
                // A second data chunk must not override the first (a small
                // legal chunk plus a forged large one could otherwise change
                // the derived duration).
                if data_len.is_some() {
                    bail!("audio has multiple data chunks");
                }
                data_len = Some(size);
            }
            _ => {}
        }
        // Chunks are word-aligned: an odd declared size carries one pad byte
        // that must be present in the buffer. Checked so a hostile size can
        // never overflow the offset walk.
        let aligned = end
            .checked_add(size % 2)
            .context("audio chunk padding overflows address space")?;
        if aligned > wav.len() {
            bail!("audio has a truncated chunk (odd size without padding at offset {offset})");
        }
        offset = aligned;
    }
    let (format, channels, sample_rate, byte_rate, block_align, bits) =
        fmt.context("audio is missing the fmt chunk")?;
    if format != 1 {
        bail!("audio is not PCM (format {format})");
    }
    if bits != 16 {
        bail!("audio is not 16-bit PCM (bits {bits})");
    }
    if channels == 0 || sample_rate == 0 || byte_rate == 0 {
        bail!("audio header declares zero channels, sample rate, or byte rate");
    }
    if sample_rate > 192_000 {
        bail!("audio sample rate {sample_rate} is implausible (>192 kHz)");
    }
    // PCM16 header consistency: the declared block align and byte rate must
    // match the channel/sample geometry exactly, and the data length must be
    // a whole number of blocks — otherwise the header is fabricated.
    if u32::from(block_align) != u32::from(channels) * 2 {
        bail!(
            "audio block align {block_align} does not match {channels} channels × 16-bit (expected {})",
            u32::from(channels) * 2
        );
    }
    let expected_byte_rate = sample_rate
        .checked_mul(u32::from(channels))
        .and_then(|value| value.checked_mul(2))
        .context("audio sample rate × channels overflows the header")?;
    if byte_rate != expected_byte_rate {
        bail!(
            "audio byte rate {byte_rate} does not match sample rate × channels × 2 ({expected_byte_rate})"
        );
    }
    let data_len = data_len.context("audio is missing the data chunk")?;
    if data_len == 0 {
        bail!("audio has an empty data chunk");
    }
    if data_len % usize::from(block_align) != 0 {
        bail!(
            "audio data length {data_len} is not aligned to the {block_align}-byte block align"
        );
    }
    let duration = Duration::from_secs_f64(data_len as f64 / f64::from(byte_rate));
    if duration > MAX_RECORDING_DURATION {
        bail!("audio duration exceeds the 30-second cap ({duration:?})");
    }
    Ok(AudioCapture {
        wav: wav.to_vec(),
        sample_rate,
        channels,
        duration,
    })
}

// ---------------------------------------------------------------------------
// Capture abstraction
// ---------------------------------------------------------------------------

/// A stream of mono 16 kHz i16 PCM chunks from the default microphone.
///
/// `next_chunk` returns `None` only when the capture ended unexpectedly
/// (device closed); normal stop is driven by the Ptt control channel or the
/// watchdog, and the pending future is simply dropped.
#[async_trait::async_trait]
pub trait CaptureBackend: Send {
    async fn next_chunk(&mut self) -> Result<Option<Vec<i16>>>;
}

/// Opens the platform capture backend. When this build was compiled without
/// the `live-capture` feature (cpal), returns an actionable error explaining
/// exactly what to install/rebuild.
pub fn open_capture() -> Result<Box<dyn CaptureBackend>> {
    #[cfg(feature = "live-capture")]
    {
        return crate::live::cpal_backend::CpalCapture::open()
            .map(|capture| Box::new(capture) as Box<dyn CaptureBackend>);
    }
    #[cfg(not(feature = "live-capture"))]
    {
        Err(anyhow!(
            "Microphone capture is not compiled into this build. Rebuild with `--features pi-coding/live-capture` (requires cpal and, on Linux, libasound2-dev), or use a build that ships it"
        ))
    }
}

// ---------------------------------------------------------------------------
// Ptt state machine
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LivePhase {
    Idle,
    Recording,
    Transcribing,
}

/// Outcome of `press()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressOutcome {
    /// A fresh recording started.
    Started,
    /// A previous press was superseded and its buffer discarded; a fresh
    /// recording started in its place (Hyper parity — the old final
    /// transcript must not land in the new target).
    Superseded,
    /// The machine is busy in a way that cannot be superseded.
    Rejected,
}

/// Outcome of `release()` / `watchdog_fire()`.
#[derive(Debug, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The utterance is worth transcribing.
    Capture(AudioCapture),
    /// Nothing usable was captured; `reason` is user-facing guidance.
    Discarded(&'static str),
}

/// A captured utterance ready for the STT client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioCapture {
    /// RIFF/WAVE bytes encoded in memory.
    pub wav: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u16,
    pub duration: Duration,
}

/// Bounded audio backlog: mono i16 PCM, oldest samples dropped first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioBuffer {
    samples: Vec<i16>,
}

impl AudioBuffer {
    fn max_samples() -> usize {
        (SAMPLE_RATE as f32 * MAX_RECORDING_SECONDS) as usize
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[must_use]
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f32(self.samples.len() as f32 / SAMPLE_RATE as f32)
    }

    fn push(&mut self, chunk: &[i16]) {
        let capacity = Self::max_samples();
        if chunk.len() >= capacity {
            self.samples.clear();
            self.samples.extend_from_slice(&chunk[chunk.len() - capacity..]);
            return;
        }
        let overflow = self
            .samples
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(capacity);
        if overflow > 0 {
            self.samples.drain(..overflow);
        }
        self.samples.extend_from_slice(chunk);
    }

    fn clear(&mut self) {
        self.samples.clear();
    }
}

/// True when the chunk carries meaningful audio (peak amplitude above
/// [`SPEECH_PEAK_THRESHOLD`]) — the no-speech watchdog's signal.
#[must_use]
pub fn speech_detected(chunk: &[i16]) -> bool {
    chunk
        .iter()
        .any(|sample| sample.unsigned_abs() > SPEECH_PEAK_THRESHOLD as u16)
}

/// Pure press/release/abort/watchdog state machine for one live session.
///
/// The machine holds no IO; the caller drives it with capture chunks and
/// wall-clock checks, and performs the STT request when `release()` yields a
/// capture.
#[derive(Debug)]
pub struct LiveMachine {
    phase: LivePhase,
    buffer: AudioBuffer,
    press_started: Option<Instant>,
    last_speech: Option<Instant>,
}

impl Default for LiveMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveMachine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            phase: LivePhase::Idle,
            buffer: AudioBuffer::default(),
            press_started: None,
            last_speech: None,
        }
    }

    #[must_use]
    pub const fn phase(&self) -> LivePhase {
        self.phase
    }

    /// Starts (or supersedes) a recording. A press while recording or
    /// transcribing supersedes the previous session: the old buffer is
    /// discarded so its transcript cannot land in the new target, and the
    /// caller must cancel any in-flight STT request for the old session.
    pub fn press(&mut self) -> PressOutcome {
        let outcome = match self.phase {
            LivePhase::Idle => PressOutcome::Started,
            LivePhase::Recording | LivePhase::Transcribing => {
                self.buffer.clear();
                PressOutcome::Superseded
            }
        };
        self.phase = LivePhase::Recording;
        self.press_started = Some(Instant::now());
        self.last_speech = None;
        outcome
    }

    /// Feeds captured audio into the bounded backlog and refreshes the
    /// no-speech deadline when the chunk carries speech. Returns `false`
    /// when the machine is not recording (the chunk is dropped).
    pub fn audio(&mut self, chunk: &[i16]) -> bool {
        if self.phase != LivePhase::Recording {
            return false;
        }
        self.buffer.push(chunk);
        if speech_detected(chunk) {
            self.last_speech = Some(Instant::now());
        }
        true
    }

    /// Whether the no-speech watchdog should fire at `now`: no speech for
    /// [`NO_SPEECH_TIMEOUT`] since the press (or since the last speech).
    #[must_use]
    pub fn watchdog_fires(&self, now: Instant) -> bool {
        if self.phase != LivePhase::Recording {
            return false;
        }
        let anchor = self.last_speech.or(self.press_started);
        anchor.is_some_and(|anchor| now.saturating_duration_since(anchor) >= NO_SPEECH_TIMEOUT)
    }

    /// Stops capture with watchdog guidance, discarding the buffer.
    pub fn watchdog_fire(&mut self) -> ReleaseOutcome {
        self.buffer.clear();
        self.phase = LivePhase::Idle;
        ReleaseOutcome::Discarded(WATCHDOG_GUIDANCE)
    }

    /// Stops capture and hands over the utterance for transcription. A
    /// capture shorter than [`MIN_UTTERANCE`] is discarded as accidental.
    pub fn release(&mut self) -> ReleaseOutcome {
        if self.phase != LivePhase::Recording {
            return ReleaseOutcome::Discarded("No active recording");
        }
        self.phase = LivePhase::Idle;
        let duration = self.buffer.duration();
        if self.buffer.is_empty() || duration < MIN_UTTERANCE {
            self.buffer.clear();
            return ReleaseOutcome::Discarded("Nothing captured — hold longer to talk");
        }
        let capture = AudioCapture {
            wav: encode_wav(&self.buffer.samples, SAMPLE_RATE, 1),
            sample_rate: SAMPLE_RATE,
            channels: 1,
            duration,
        };
        self.buffer.clear();
        ReleaseOutcome::Capture(capture)
    }

    /// Drops the capture (Esc / TUI teardown) without transcribing.
    pub fn abort(&mut self) {
        self.buffer.clear();
        self.phase = LivePhase::Idle;
    }
}

// ---------------------------------------------------------------------------
// STT client (OpenAI-compatible transcriptions endpoint)
// ---------------------------------------------------------------------------

/// Minimal multipart/form-data body builder for the STT request. Hand-built
/// (rather than reqwest's `multipart` feature) so the shared workspace
/// reqwest stays lean and the exact wire shape is unit-testable.
struct MultipartBody {
    boundary: String,
    bytes: Vec<u8>,
}

impl MultipartBody {
    fn new() -> Self {
        Self {
            boundary: format!("----rpiLiveBoundary{}", uuid::Uuid::new_v4().simple()),
            bytes: Vec::new(),
        }
    }

    fn push_field(&mut self, name: &str, value: &str) {
        self.bytes.extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        self.bytes.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.extend_from_slice(b"\r\n");
    }

    fn push_file(&mut self, name: &str, filename: &str, content_type: &str, content: &[u8]) {
        self.bytes.extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        self.bytes.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n").as_bytes(),
        );
        self.bytes.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
        self.bytes.extend_from_slice(content);
        self.bytes.extend_from_slice(b"\r\n");
    }

    fn finish(mut self) -> (String, Vec<u8>) {
        self.bytes.extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        (self.boundary, self.bytes)
    }
}

/// HTTP client for `POST {base}/v1/audio/transcriptions`.
#[derive(Clone)]
pub struct SttClient {
    http: reqwest::Client,
}

impl SttClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(STT_REQUEST_TIMEOUT)
            .build()
            .context("building STT HTTP client")?;
        Ok(Self { http })
    }

    /// Transcribes an in-memory WAV through the configured endpoint and
    /// returns the transcript text. Errors are actionable and redacted: the
    /// bearer key is never included, and server-echoed secrets are scrubbed.
    pub async fn transcribe(
        &self,
        settings: &LiveRuntimeSettings,
        capture: &AudioCapture,
    ) -> Result<String> {
        validate_live_settings(settings)?;
        let url = transcriptions_url(&settings.stt_base_url);
        let mut body = MultipartBody::new();
        body.push_file("file", AUDIO_FILENAME, "audio/wav", &capture.wav);
        body.push_field("model", &settings.stt_model);
        if let Some(language) = settings.language.as_deref().filter(|value| !value.trim().is_empty()) {
            body.push_field("language", language);
        }
        let (boundary, bytes) = body.finish();
        let mut response = self
            .http
            .post(&url)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", settings.stt_api_key))
            .body(bytes)
            .send()
            .await
            .context("STT request failed")?;
        let status = response.status();
        // Bound the response BEFORE reading: a Content-Length declared over
        // the cap fails immediately, and the body is otherwise streamed in
        // chunks with a hard byte cap so a hostile endpoint can never
        // allocate unbounded memory or project an oversized transcript into
        // the RPC frame / composer.
        if let Some(length) = response.content_length() {
            if length > STT_MAX_RESPONSE_BYTES as u64 {
                bail!(
                    "STT response of {length} bytes exceeds the {STT_MAX_RESPONSE_BYTES}-byte cap"
                );
            }
        }
        let mut bytes = Vec::with_capacity(1024);
        loop {
            let chunk = response.chunk().await.context("reading STT response body")?;
            let Some(chunk) = chunk else {
                break;
            };
            if bytes.len().saturating_add(chunk.len()) > STT_MAX_RESPONSE_BYTES {
                bail!(
                    "STT response exceeds the {STT_MAX_RESPONSE_BYTES}-byte cap"
                );
            }
            bytes.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&bytes);
        if !status.is_success() {
            // The endpoint URL is deliberately NOT echoed (the browser must
            // never learn where the STT endpoint lives); credential shapes
            // the server may echo back are redacted and the snippet is
            // bounded so a hostile or verbose body cannot project unbounded
            // text.
            return Err(anyhow!(
                "STT endpoint returned {status}: {}",
                redact_bounded(&text, STT_ERROR_SNIPPET_CHARS)
            ));
        }
        parse_transcript_response(&text)
    }
}

/// Extracts `text` from an OpenAI-compatible transcriptions response
/// (`{"text": "..."}`). A server-side `error` field becomes an actionable
/// error; the text is trimmed and returned verbatim otherwise. Every echoed
/// body/message is redacted AND bounded so a hostile endpoint can never
/// project unbounded text into an RPC response.
fn parse_transcript_response(body: &str) -> Result<String> {
    let value: Value = serde_json::from_str(body).with_context(|| {
        format!(
            "STT endpoint returned non-JSON response: {}",
            redact_bounded(body, STT_ERROR_SNIPPET_CHARS)
        )
    })?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown STT error");
        return Err(anyhow!(
            "STT endpoint error: {}",
            redact_bounded(message, STT_ERROR_SNIPPET_CHARS)
        ));
    }
    value
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| text.chars().take(STT_MAX_TRANSCRIPT_CHARS).collect::<String>())
        .ok_or_else(|| {
            anyhow!(
                "STT endpoint response had no `text` field: {}",
                redact_bounded(body, STT_ERROR_SNIPPET_CHARS)
            )
        })
}

// ---------------------------------------------------------------------------
// Session pipeline
// ---------------------------------------------------------------------------

/// Commands the TUI sends to an active Ptt session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PttControl {
    /// The user released the hold key: stop capture and transcribe.
    Release,
    /// Drop the capture (Esc / teardown): stop without transcribing.
    Abort,
}

/// Events a Ptt session emits to the TUI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PttSessionEvent {
    /// Capture started; the status line shows `Recording…`.
    Started,
    /// Capture stopped without a transcript (abort, watchdog, empty capture).
    /// `reason` is user-facing guidance.
    Stopped { reason: String },
    /// Final transcript ready to land in the composer draft (never
    /// auto-submitted — the user reviews and presses Enter).
    Transcript { text: String },
    /// Actionable, redacted error (no microphone, TLS rejection, STT failure).
    Error { message: String },
}

/// Runs one press→release Ptt session: opens the mic, records into the
/// bounded backlog under the no-speech watchdog, then transcribes on release
/// (or drops on abort/supersede). Sends exactly one terminal event.
///
/// Takes the [`SttClient`] by value so the caller can spawn the session as a
/// `'static` task.
pub async fn run_ptt_session(
    settings: LiveRuntimeSettings,
    stt: SttClient,
    capture: Box<dyn CaptureBackend>,
    mut control: mpsc::UnboundedReceiver<PttControl>,
    events: mpsc::UnboundedSender<PttSessionEvent>,
    watchdog: Duration,
) {
    let mut machine = LiveMachine::new();
    machine.press();
    let _ = events.send(PttSessionEvent::Started);
    let mut capture = capture;
    loop {
        let now = Instant::now();
        if machine.watchdog_fires(now) {
            let outcome = machine.watchdog_fire();
            let reason = match outcome {
                ReleaseOutcome::Discarded(reason) => reason.to_owned(),
                ReleaseOutcome::Capture(_) => WATCHDOG_GUIDANCE.to_owned(),
            };
            let _ = events.send(PttSessionEvent::Stopped { reason });
            return;
        }
        tokio::select! {
            chunk = capture.next_chunk() => match chunk {
                Ok(Some(chunk)) => {
                    machine.audio(&chunk);
                }
                Ok(None) => {
                    let _ = events.send(PttSessionEvent::Error {
                        message: "Capture ended unexpectedly (device closed)".to_owned(),
                    });
                    return;
                }
                Err(error) => {
                    let _ = events.send(PttSessionEvent::Error {
                        message: format!("Capture ended unexpectedly: {error:#}"),
                    });
                    return;
                }
            },
            command = control.recv() => match command {
                Some(PttControl::Release) => match machine.release() {
                    ReleaseOutcome::Capture(capture) => {
                        match stt.transcribe(&settings, &capture).await {
                            Ok(text) => {
                                let _ = events.send(PttSessionEvent::Transcript { text });
                            }
                            Err(error) => {
                                let _ = events.send(PttSessionEvent::Error {
                                    message: redact_secrets(&format!("{error:#}")),
                                });
                            }
                        }
                        return;
                    }
                    ReleaseOutcome::Discarded(reason) => {
                        let _ = events.send(PttSessionEvent::Stopped {
                            reason: reason.to_owned(),
                        });
                        return;
                    }
                },
                Some(PttControl::Abort) | None => {
                    machine.abort();
                    let _ = events.send(PttSessionEvent::Stopped {
                        reason: "Aborted".to_owned(),
                    });
                    return;
                }
            },
            _ = tokio::time::sleep(watchdog) => {
                let outcome = machine.watchdog_fire();
                let reason = match outcome {
                    ReleaseOutcome::Discarded(reason) => reason.to_owned(),
                    ReleaseOutcome::Capture(_) => WATCHDOG_GUIDANCE.to_owned(),
                };
                let _ = events.send(PttSessionEvent::Stopped { reason });
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Delegation bridge (spoken coding task → bound agent turn)
// ---------------------------------------------------------------------------

/// Coding-task verbs that mark a transcribed instruction as delegatable when
/// paired with a code-domain signal (see [`is_delegation_candidate`]).
///
/// Hyper's Codex Live receives server-side `delegation.created` events from
/// the voice model; rpi's generic STT cannot, so the bridge detects the
/// intent client-side from the transcript text.
pub const DELEGATION_VERBS: &[&str] = &[
    "implement", "fix", "add", "test", "refactor", "write", "create",
    "update", "change", "remove", "delete", "debug", "repair", "migrate",
    "optimize", "rewrite", "extend", "port",
];

/// Code-domain nouns that pair with a verb to mark a delegation candidate.
const DELEGATION_CODE_WORDS: &[&str] = &[
    "function", "class", "struct", "enum", "trait", "module", "crate", "api",
    "endpoint", "cli", "library", "package", "dependency", "import", "build",
    "bug", "crash", "panic", "error", "test", "code", "script", "config",
    "schema", "database", "server", "client", "implementation", "interface",
];

/// File-extension signals (with the leading dot) that mark code-domain text.
const DELEGATION_EXTENSIONS: &[&str] = &[
    ".rs", ".py", ".ts", ".js", ".go", ".c", ".cpp", ".h", ".java", ".rb",
    ".php", ".toml", ".json", ".yaml", ".yml", ".md", ".sh", ".sql", ".html",
    ".css", ".lock",
];

/// True when `text` reads like a coding task worth delegating to the bound
/// agent: a coding verb (implement/fix/add/test/refactor/…) paired with a
/// code-domain signal (a file path, a known file extension, or a code word).
///
/// This is the rpi substitute for Hyper's server-side `delegation.created`
/// event (analysis §7.3): it only ever *hints* the UI — the delegation still
/// runs through the ordinary prompt path — so conservative false positives
/// are harmless and the matcher is deliberately lexical and deterministic.
#[must_use]
pub fn is_delegation_candidate(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    let Some(verb_position) = DELEGATION_VERBS
        .iter()
        .find_map(|verb| find_word(&lower, verb))
    else {
        return false;
    };
    has_code_signal(&lower, verb_position)
}

/// Whether lowercased text carries any code-domain signal (word, extension,
/// or path separator). Word signals must not coincide with the verb itself —
/// "test" is both a verb and a code noun, so "test the microphone" must not
/// read as a coding task while "test the function" and "add tests" do.
fn has_code_signal(lower: &str, verb_position: usize) -> bool {
    let word_signal = DELEGATION_CODE_WORDS.iter().any(|word| {
        find_code_word(lower, word).is_some_and(|position| position != verb_position)
    });
    word_signal
        || DELEGATION_EXTENSIONS
            .iter()
            .any(|extension| lower.contains(extension))
        || lower.contains(['/', '\\'])
}

/// Code-word match with a simple-English-plural tolerance: "tests" matches
/// the code word "test". Voice transcripts of commands ("add tests") almost
/// always pluralize the noun. Allocation-free: the plural is matched by
/// inspecting the character after the singular match. Returns the byte
/// offset of the match (for the verb-coincidence check) or `None`.
fn find_code_word(lower: &str, word: &str) -> Option<usize> {
    if let Some(position) = find_word(lower, word) {
        return Some(position);
    }
    let mut search_from = 0;
    while let Some(relative) = lower[search_from..].find(word) {
        let start = search_from + relative;
        let end = start + word.len();
        if lower[end..].starts_with('s') {
            let before = lower[..start].chars().next_back();
            let after_plural = lower[end + 1..].chars().next();
            if !before.is_some_and(char::is_alphanumeric)
                && !after_plural.is_some_and(char::is_alphanumeric)
            {
                return Some(start);
            }
        }
        search_from = end;
    }
    None
}

/// Word-boundary substring match: `needle` must be flanked by
/// non-alphanumeric characters (or the text edges), so "fix" does not match
/// inside "affixes" or "fixer" (the following 'e' is not a boundary).
/// Returns the byte offset of the first match, or `None`.
fn find_word(haystack: &str, needle: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(relative) = haystack[search_from..].find(needle) {
        let start = search_from + relative;
        let end = start + needle.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        let before_boundary = !before.is_some_and(char::is_alphanumeric);
        let after_boundary = !after.is_some_and(char::is_alphanumeric);
        if before_boundary && after_boundary {
            return Some(start);
        }
        search_from = end;
    }
    None
}

// ---------------------------------------------------------------------------
// cpal capture backend (feature-gated: requires ALSA dev headers on Linux)
// ---------------------------------------------------------------------------

#[cfg(feature = "live-capture")]
pub(crate) mod cpal_backend {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    /// Captures mono i16 PCM from the default input device, preferring
    /// 16 kHz and falling back to the device's default rate when 16 kHz is
    /// unsupported (the WAV carries the actual rate).
    pub struct CpalCapture {
        stream: cpal::Stream,
        chunks: mpsc::UnboundedReceiver<Vec<i16>>,
    }

    impl CpalCapture {
        pub fn open() -> Result<Self> {
            let host = cpal::default_host();
            let device = host
                .default_input_device()
                .ok_or_else(|| anyhow!("No microphone found — connect a microphone and try again"))?;
            let default_config = device
                .default_input_config()
                .context("reading default microphone format")?;
            let sample_format = default_config.sample_format();
            let channels = 1u16;
            let (tx, rx) = mpsc::unbounded_channel();
            let mut errors: Vec<String> = Vec::new();
            let mut stream = None;
            // Prefer 16 kHz (the pipeline contract); fall back to the
            // device's default rate so exotic hardware still works.
            let mut rates = vec![cpal::SampleRate(SAMPLE_RATE)];
            if default_config.sample_rate().0 != SAMPLE_RATE {
                rates.push(default_config.sample_rate());
            }
            for rate in rates {
                let config = cpal::StreamConfig {
                    channels,
                    sample_rate: rate,
                    buffer_size: cpal::BufferSize::Default,
                };
                let tx = tx.clone();
                let error_tx = tx.clone();
                let build = match sample_format {
                    cpal::SampleFormat::I16 => build_stream::<i16>(&device, &config, tx, error_tx),
                    cpal::SampleFormat::F32 => build_stream::<f32>(&device, &config, tx, error_tx),
                    cpal::SampleFormat::U16 => build_stream::<u16>(&device, &config, tx, error_tx),
                    other => {
                        return Err(anyhow!(
                            "Unsupported microphone sample format {other:?}; /live needs I16, F32, or U16"
                        ));
                    }
                };
                match build {
                    Ok(built) => {
                        stream = Some(built);
                        break;
                    }
                    Err(error) => {
                        errors.push(format!("{rate} Hz: {error}"));
                    }
                }
            }
            let Some(stream) = stream else {
                return Err(anyhow!("Could not open the microphone: {}", errors.join("; ")));
            };
            stream.play().context("starting microphone capture")?;
            Ok(Self { stream, chunks: rx })
        }
    }

    fn build_stream<T>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        tx: mpsc::UnboundedSender<Vec<i16>>,
        error_tx: mpsc::UnboundedSender<Vec<i16>>,
    ) -> Result<cpal::Stream>
    where
        T: cpal::SizedSample + cpal::Sample + Send + 'static,
    {
        let data_callback = move |data: &[T], _: &cpal::InputCallbackInfo| {
            let converted = data
                .iter()
                .map(|sample| sample.to_sample::<i16>())
                .collect::<Vec<i16>>();
            let _ = tx.send(converted);
        };
        let error_callback = move |error: cpal::StreamError| {
            // Surface runtime device errors by closing the chunk stream so
            // the pipeline reports an actionable stop.
            let _ = error_tx.send(Vec::new());
            let _ = error;
        };
        device
            .build_input_stream(config, data_callback, error_callback, None)
            .context("opening the microphone stream")
    }

    #[async_trait::async_trait]
    impl CaptureBackend for CpalCapture {
        async fn next_chunk(&mut self) -> Result<Option<Vec<i16>>> {
            match self.chunks.recv().await {
                Some(chunk) if chunk.is_empty() => Err(anyhow!("microphone stream error")),
                Some(chunk) => Ok(Some(chunk)),
                None => Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use futures_util::future;
    use tokio::sync::mpsc;

    use super::*;

    fn settings(base: &str) -> LiveRuntimeSettings {
        LiveRuntimeSettings {
            enabled: true,
            mode: "stt".to_owned(),
            stt_base_url: base.to_owned(),
            stt_api_key: "test-live-key".to_owned(),
            stt_model: "whisper-1".to_owned(),
            realtime_base_url: String::new(),
            realtime_api_key: String::new(),
            realtime_model: "gpt-realtime-1.5".to_owned(),
            voice: "sol".to_owned(),
            language: None,
            allow_insecure: true,
        }
    }

    fn speech_chunk() -> Vec<i16> {
        // 4800 samples @ 16 kHz = 0.3 s, above MIN_UTTERANCE on its own.
        std::iter::repeat(2_000i16)
            .take(4_800)
            .collect::<Vec<i16>>()
    }

    fn silence_chunk() -> Vec<i16> {
        vec![0i16; 3_200]
    }

    // ------------------------------------------------------------------
    // Settings validation / TLS policy
    // ------------------------------------------------------------------

    #[test]
    fn unconfigured_settings_produce_actionable_errors() {
        let mut s = settings("https://stt.example");
        s.enabled = false;
        let error = validate_live_settings(&s).unwrap_err().to_string();
        assert!(error.contains("Settings.live.enabled"), "{error}");

        s.enabled = true;
        s.stt_base_url.clear();
        let error = validate_live_settings(&s).unwrap_err().to_string();
        assert!(error.contains("Settings.live.sttBaseUrl"), "{error}");

        s.stt_base_url = "https://stt.example".to_owned();
        s.stt_api_key.clear();
        let error = validate_live_settings(&s).unwrap_err().to_string();
        assert!(error.contains("Settings.live.sttApiKey"), "{error}");
    }

    #[test]
    fn http_endpoint_rejected_unless_allow_insecure() {
        // The settings helper defaults allowInsecure=true (needed by the mock
        // server tests); this test explicitly disables it.
        let s = settings("http://127.0.0.1:9000");
        let s = LiveRuntimeSettings {
            allow_insecure: false,
            ..s
        };
        let error = validate_live_settings(&s).unwrap_err().to_string();
        assert!(error.contains("allowInsecure"), "{error}");
        assert!(error.contains("https"), "{error}");

        let mut insecure = s;
        insecure.allow_insecure = true;
        assert!(validate_live_settings(&insecure).is_ok());
    }

    #[test]
    fn websocket_urls_always_rejected() {
        for scheme in ["ws", "wss"] {
            let s = settings(&format!("{scheme}://stt.example/v1"));
            let error = validate_live_settings(&s).unwrap_err().to_string();
            assert!(error.contains("http"), "ws scheme rejected for {scheme}: {error}");
            assert!(error.contains("audio/transcriptions"), "{error}");
        }
    }

    #[test]
    fn realtime_mode_directs_to_web_listener_not_stt_base_url() {
        // A fully-configured realtime config (base url + key present) must not
        // fall through to the STT-base-url checks: the TUI does not implement
        // WebRTC, so the error points at the Web listener instead.
        let s = LiveRuntimeSettings {
            mode: "realtime".to_owned(),
            realtime_base_url: "http://localhost:8317".to_owned(),
            realtime_api_key: "rt-key".to_owned(),
            // STT deliberately unconfigured to prove it is not mentioned.
            stt_base_url: String::new(),
            stt_api_key: String::new(),
            ..settings("https://stt.example")
        };
        let error = validate_live_settings(&s).unwrap_err().to_string();
        assert!(error.contains("realtime"), "{error}");
        assert!(error.contains("rpi --listen 127.0.0.1:8080"), "must give a valid listen command: {error}");
        assert!(error.contains("/web"), "must point at the /web page: {error}");
        assert!(!error.contains("/live page"), "must not reference a /live web page: {error}");
        assert!(
            !error.contains("sttBaseUrl"),
            "realtime must not misreport a missing sttBaseUrl: {error}"
        );
        assert!(
            !error.contains("sttApiKey"),
            "realtime must not mention sttApiKey: {error}"
        );
    }

    #[test]
    fn realtime_mode_errors_even_when_stt_is_configured() {
        // Even with a valid STT endpoint, realtime mode never arms the TUI.
        let s = LiveRuntimeSettings {
            mode: "realtime".to_owned(),
            ..settings("https://stt.example")
        };
        let error = validate_live_settings(&s).unwrap_err().to_string();
        assert!(error.contains("Web listener"), "{error}");
        assert!(validate_live_settings(&s).is_err());
    }

    #[test]
    fn unknown_mode_fails_fast_listing_legal_values() {
        let s = LiveRuntimeSettings {
            mode: "banana".to_owned(),
            ..settings("https://stt.example")
        };
        let error = validate_live_settings(&s).unwrap_err().to_string();
        assert!(error.contains("banana"), "unknown mode echoed: {error}");
        assert!(error.contains("stt"), "{error}");
        assert!(error.contains("realtime"), "{error}");
        // Must not reach the STT-base-url checks.
        assert!(!error.contains("sttBaseUrl"), "{error}");
    }

    #[test]
    fn empty_mode_fails_fast_listing_legal_values() {
        let s = LiveRuntimeSettings {
            mode: "   ".to_owned(),
            ..settings("https://stt.example")
        };
        let error = validate_live_settings(&s).unwrap_err().to_string();
        assert!(error.contains("live.mode"), "{error}");
        assert!(error.contains("stt"), "{error}");
        assert!(error.contains("realtime"), "{error}");
    }

    #[test]
    fn mode_is_trimmed_and_case_insensitive() {
        // Mixed-case/whitespace "stt" canonicalizes to the STT branch and
        // validates against the configured STT endpoint.
        for mode in ["STT", "Stt", "  stt  ", " sTt "] {
            let s = LiveRuntimeSettings {
                mode: mode.to_owned(),
                ..settings("https://stt.example")
            };
            assert!(
                validate_live_settings(&s).is_ok(),
                "mode `{mode}` should canonicalize to stt and validate"
            );
        }
        // Mixed-case/whitespace "realtime" canonicalizes to the realtime
        // branch and directs to the Web listener (never the STT checks).
        for mode in ["REALTIME", "Realtime", "  realtime  ", " rEaLtImE "] {
            let s = LiveRuntimeSettings {
                mode: mode.to_owned(),
                stt_base_url: String::new(),
                stt_api_key: String::new(),
                ..settings("https://stt.example")
            };
            let error = validate_live_settings(&s).unwrap_err().to_string();
            assert!(
                error.contains("Web listener"),
                "mode `{mode}` should canonicalize to realtime: {error}"
            );
            assert!(
                error.contains("rpi --listen 127.0.0.1:8080"),
                "mode `{mode}` must give a valid listen command: {error}"
            );
            assert!(
                error.contains("/web"),
                "mode `{mode}` must point at the /web page: {error}"
            );
            assert!(
                !error.contains("sttBaseUrl"),
                "mode `{mode}` must not misreport sttBaseUrl: {error}"
            );
        }
    }

    #[test]
    fn stt_mode_still_validates_endpoint_and_key() {
        // Regression guard: the canonical STT path is unchanged by the mode
        // dispatch — empty base url / key / plaintext scheme still error.
        let mut s = settings("https://stt.example");
        s.stt_base_url.clear();
        assert!(validate_live_settings(&s).is_err());
        s.stt_base_url = "https://stt.example".to_owned();
        s.stt_api_key.clear();
        assert!(validate_live_settings(&s).is_err());
        s.stt_api_key = "key".to_owned();
        assert!(validate_live_settings(&s).is_ok());
    }

    #[test]
    fn transcriptions_url_joins_and_deduplicates_v1() {
        assert_eq!(
            transcriptions_url("https://stt.example"),
            "https://stt.example/v1/audio/transcriptions"
        );
        assert_eq!(
            transcriptions_url("https://stt.example/"),
            "https://stt.example/v1/audio/transcriptions"
        );
        assert_eq!(
            transcriptions_url("https://stt.example/v1"),
            "https://stt.example/v1/audio/transcriptions"
        );
    }

    // ------------------------------------------------------------------
    // WAV encoding
    // ------------------------------------------------------------------

    #[test]
    fn wav_encoding_produces_valid_riff_header() {
        let pcm = vec![0i16, 100, -100, 32767, -32768];
        let wav = encode_wav(&pcm, 16_000, 1);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1, "PCM format");
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1, "mono");
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            16_000
        );
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + pcm.len() * 2);
    }

    #[test]
    fn parse_wav_capture_round_trips_encode_wav() {
        let pcm = vec![0i16, 100, -100, 32767, -32768];
        let wav = encode_wav(&pcm, 16_000, 1);
        let capture = parse_wav_capture(&wav).expect("valid wav");
        assert_eq!(capture.sample_rate, 16_000);
        assert_eq!(capture.channels, 1);
        // duration = data_len / byte_rate = (pcm.len() * 2) / (rate * ch * 2)
        //          = pcm.len() / rate.
        assert_eq!(
            capture.duration,
            Duration::from_secs_f64(pcm.len() as f64 / 16_000.0)
        );
        // The forwarded bytes are the verbatim input (the STT endpoint sees
        // exactly what the client sent).
        assert_eq!(capture.wav, wav);
    }

    #[test]
    fn parse_wav_capture_tolerates_chunks_before_data() {
        // Insert a LIST chunk between fmt and data; the chunk walk must find
        // both regardless of order.
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&52u32.to_le_bytes()); // file size - 8 = 60 - 8
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&32_000u32.to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // 16-bit
        wav.extend_from_slice(b"LIST");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(b"INFO");
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 4]);
        let capture = parse_wav_capture(&wav).expect("chunked wav");
        assert_eq!(capture.sample_rate, 16_000);
        assert_eq!(capture.channels, 1);
        assert_eq!(capture.duration, Duration::from_secs_f64(4.0 / 32_000.0));
    }

    #[test]
    fn parse_wav_capture_rejects_non_wav() {
        let error = parse_wav_capture(b"not a wav file at all").expect_err("must reject");
        assert!(error.to_string().contains("RIFF/WAVE"), "{error}");
        // Truncated below the canonical header.
        let mut short = encode_wav(&[0i16; 4], 16_000, 1);
        short.truncate(20);
        let error = parse_wav_capture(&short).expect_err("must reject");
        assert!(error.to_string().contains("RIFF/WAVE"), "{error}");
    }

    #[test]
    fn parse_wav_capture_rejects_non_pcm_and_non_16bit() {
        let mut wav = encode_wav(&[0i16; 8], 16_000, 1);
        wav[20..22].copy_from_slice(&2u16.to_le_bytes()); // format 2 (non-PCM)
        let error = parse_wav_capture(&wav).expect_err("must reject format 2");
        assert!(error.to_string().contains("not PCM"), "{error}");

        let mut wav = encode_wav(&[0i16; 8], 16_000, 1);
        wav[34..36].copy_from_slice(&8u16.to_le_bytes()); // 8-bit
        let error = parse_wav_capture(&wav).expect_err("must reject 8-bit");
        assert!(error.to_string().contains("16-bit"), "{error}");
    }

    #[test]
    fn parse_wav_capture_rejects_empty_data_and_bad_rates() {
        let mut wav = encode_wav(&[0i16; 8], 16_000, 1);
        wav[40..44].copy_from_slice(&0u32.to_le_bytes()); // empty data chunk
        let error = parse_wav_capture(&wav).expect_err("must reject empty data");
        assert!(error.to_string().contains("empty data"), "{error}");

        let mut wav = encode_wav(&[0i16; 8], 16_000, 1);
        wav[24..28].copy_from_slice(&0u32.to_le_bytes()); // zero sample rate
        let error = parse_wav_capture(&wav).expect_err("must reject zero rate");
        assert!(error.to_string().contains("zero"), "{error}");

        let mut wav = encode_wav(&[0i16; 8], 16_000, 1);
        wav[24..28].copy_from_slice(&1_000_000u32.to_le_bytes()); // implausible rate
        let error = parse_wav_capture(&wav).expect_err("must reject absurd rate");
        assert!(error.to_string().contains("implausible"), "{error}");
    }

    #[test]
    fn parse_wav_capture_rejects_truncated_and_oversized_chunks() {
        // Buffer cut mid-data: the RIFF header still declares the full file
        // size, so the declared-size check fails closed before any walk.
        let mut truncated = encode_wav(&[0i16; 16], 16_000, 1);
        truncated.truncate(44 + 4);
        let error = parse_wav_capture(&truncated).expect_err("must reject truncated data");
        assert!(error.to_string().contains("truncated"), "{error}");

        // A data chunk whose declared size exceeds the buffer (hostile size
        // field) is a truncated chunk, never a clamped pass.
        let mut oversized = encode_wav(&[0i16; 16], 16_000, 1);
        oversized[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        let error = parse_wav_capture(&oversized).expect_err("must reject oversized chunk");
        assert!(error.to_string().contains("truncated chunk"), "{error}");

        // Trailing bytes beyond the declared RIFF size (a declared size
        // SMALLER than the buffer) fail closed — the canonical browser
        // encoder never emits trailing data.
        let mut trailing = encode_wav(&[0i16; 16], 16_000, 1);
        trailing.extend_from_slice(b"EXTRA");
        let error = parse_wav_capture(&trailing).expect_err("must reject trailing bytes");
        assert!(error.to_string().contains("trailing bytes"), "{error}");
    }

    #[test]
    fn parse_wav_capture_rejects_duplicate_fmt_and_data_chunks() {
        // Two data chunks: the second must not silently override the first
        // (a small legal chunk plus a forged large one could otherwise change
        // the derived duration).
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&52u32.to_le_bytes()); // 60 - 8
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&32_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 4]);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 4]);
        let error = parse_wav_capture(&wav).expect_err("must reject duplicate data");
        assert!(error.to_string().contains("multiple data chunks"), "{error}");

        // Two fmt chunks: the second must not override the first.
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&64u32.to_le_bytes()); // 72 - 8
        wav.extend_from_slice(b"WAVE");
        for _ in 0..2 {
            wav.extend_from_slice(b"fmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
            wav.extend_from_slice(&1u16.to_le_bytes()); // mono
            wav.extend_from_slice(&16_000u32.to_le_bytes());
            wav.extend_from_slice(&32_000u32.to_le_bytes());
            wav.extend_from_slice(&2u16.to_le_bytes());
            wav.extend_from_slice(&16u16.to_le_bytes());
        }
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 4]);
        let error = parse_wav_capture(&wav).expect_err("must reject duplicate fmt");
        assert!(error.to_string().contains("multiple fmt chunks"), "{error}");
    }

    #[test]
    fn parse_wav_capture_rejects_byte_rate_overflow() {
        // A consistent-but-extreme header whose channels × sample rate
        // overflows u32 (32767 × 192000 × 2): the checked multiplication
        // must fail with an error, never panic (debug builds would abort on
        // the unchecked product).
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&40u32.to_le_bytes()); // 48 - 8
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&32767u16.to_le_bytes()); // channels
        wav.extend_from_slice(&192_000u32.to_le_bytes()); // sample rate
        wav.extend_from_slice(&1u32.to_le_bytes()); // byte rate (non-zero)
        wav.extend_from_slice(&65_534u16.to_le_bytes()); // block align = ch × 2
        wav.extend_from_slice(&16u16.to_le_bytes()); // 16-bit
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 4]);
        let error = parse_wav_capture(&wav).expect_err("must reject overflow");
        assert!(error.to_string().contains("overflows"), "{error}");
    }

    #[test]
    fn parse_wav_capture_enforces_word_alignment_padding() {
        // An odd-sized chunk (LIST, size 3) with its pad byte followed by the
        // data chunk parses; without the pad byte the walk misaligns and the
        // odd-size chunk fails closed.
        let mut with_pad = Vec::new();
        with_pad.extend_from_slice(b"RIFF");
        with_pad.extend_from_slice(&52u32.to_le_bytes());
        with_pad.extend_from_slice(b"WAVE");
        with_pad.extend_from_slice(b"fmt ");
        with_pad.extend_from_slice(&16u32.to_le_bytes());
        with_pad.extend_from_slice(&1u16.to_le_bytes()); // PCM
        with_pad.extend_from_slice(&1u16.to_le_bytes()); // mono
        with_pad.extend_from_slice(&16_000u32.to_le_bytes());
        with_pad.extend_from_slice(&32_000u32.to_le_bytes()); // byte rate
        with_pad.extend_from_slice(&2u16.to_le_bytes()); // block align
        with_pad.extend_from_slice(&16u16.to_le_bytes()); // 16-bit
        with_pad.extend_from_slice(b"LIST");
        with_pad.extend_from_slice(&3u32.to_le_bytes());
        with_pad.extend_from_slice(b"abc");
        with_pad.push(0); // word-align pad byte
        with_pad.extend_from_slice(b"data");
        with_pad.extend_from_slice(&4u32.to_le_bytes());
        with_pad.extend_from_slice(&[0u8; 4]);
        let capture = parse_wav_capture(&with_pad).expect("odd chunk with pad parses");
        assert_eq!(capture.sample_rate, 16_000);
        assert_eq!(capture.duration, Duration::from_secs_f64(4.0 / 32_000.0));

        // Same layout WITHOUT the pad byte: the file is now 59 bytes against
        // a declared RIFF size of 60 — the exact-length check fails closed
        // (and the walk would misalign on the odd chunk either way).
        let mut without_pad = with_pad.clone();
        without_pad.remove(with_pad.len() - 13); // remove the pad byte
        let error = parse_wav_capture(&without_pad).expect_err("must reject missing pad");
        assert!(error.to_string().contains("truncated"), "{error}");
    }

    #[test]
    fn parse_wav_capture_rejects_inconsistent_header_geometry() {
        // byte_rate not equal to sample_rate × channels × 2.
        let mut wav = encode_wav(&[0i16; 16], 16_000, 1);
        wav[28..32].copy_from_slice(&99_999u32.to_le_bytes());
        let error = parse_wav_capture(&wav).expect_err("must reject bad byte rate");
        assert!(error.to_string().contains("byte rate"), "{error}");

        // block_align not equal to channels × 2.
        let mut wav = encode_wav(&[0i16; 16], 16_000, 1);
        wav[32..34].copy_from_slice(&7u16.to_le_bytes());
        let error = parse_wav_capture(&wav).expect_err("must reject bad block align");
        assert!(error.to_string().contains("block align"), "{error}");

        // Data length not a whole number of blocks (odd byte count).
        let mut wav = encode_wav(&[0i16; 16], 16_000, 1);
        wav[40..44].copy_from_slice(&5u32.to_le_bytes());
        let error = parse_wav_capture(&wav).expect_err("must reject misaligned data");
        assert!(error.to_string().contains("not aligned"), "{error}");
    }

    #[test]
    fn parse_wav_capture_rejects_duration_beyond_cap_by_header() {
        // A consistent but low-rate header: 1 kHz mono PCM16 with a large
        // data chunk yields 50 s of audio — well within the byte bound but
        // far past the 30-second utterance cap. The byte cap alone cannot
        // catch this; the duration check must.
        let data_len = 100_000usize; // 50 s at 2000 bytes/s
        let mut wav = Vec::with_capacity(44 + data_len);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&1_000u32.to_le_bytes()); // sample rate
        wav.extend_from_slice(&2_000u32.to_le_bytes()); // byte rate = rate × ch × 2
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // 16-bit
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data_len as u32).to_le_bytes());
        wav.extend_from_slice(&vec![0u8; data_len]);
        let error = parse_wav_capture(&wav).expect_err("must reject >30s duration");
        assert!(error.to_string().contains("30-second cap"), "{error}");
    }

    // ------------------------------------------------------------------
    // State machine transitions
    // ------------------------------------------------------------------

    #[test]
    fn press_recording_release_transcribes() {
        let mut machine = LiveMachine::new();
        assert_eq!(machine.press(), PressOutcome::Started);
        assert_eq!(machine.phase(), LivePhase::Recording);
        assert!(machine.audio(&speech_chunk()));
        let outcome = machine.release();
        let ReleaseOutcome::Capture(capture) = outcome else {
            panic!("expected capture, got {outcome:?}");
        };
        assert_eq!(&capture.wav[..4], b"RIFF");
        assert_eq!(capture.sample_rate, 16_000);
        assert_eq!(capture.channels, 1);
        assert!(capture.duration >= MIN_UTTERANCE);
        assert_eq!(machine.phase(), LivePhase::Idle);
    }

    #[test]
    fn release_without_speech_is_discarded() {
        let mut machine = LiveMachine::new();
        machine.press();
        assert!(machine.audio(&silence_chunk()));
        let outcome = machine.release();
        assert!(matches!(outcome, ReleaseOutcome::Discarded(_)));
        assert_eq!(machine.phase(), LivePhase::Idle);
    }

    #[test]
    fn press_supersedes_active_recording() {
        let mut machine = LiveMachine::new();
        assert_eq!(machine.press(), PressOutcome::Started);
        machine.audio(&speech_chunk());
        assert_eq!(machine.press(), PressOutcome::Superseded);
        assert_eq!(machine.phase(), LivePhase::Recording);
        // The superseded buffer was discarded: a fresh capture contains only
        // the new utterance.
        machine.audio(&speech_chunk());
        let ReleaseOutcome::Capture(capture) = machine.release() else {
            panic!("expected capture after supersede");
        };
        assert_eq!(capture.duration, Duration::from_secs_f32(0.3));
    }

    #[test]
    fn watchdog_timeout_stops_capture_with_guidance() {
        let mut machine = LiveMachine::new();
        machine.press();
        assert!(!machine.watchdog_fires(Instant::now() + Duration::from_secs(9)));
        assert!(machine.watchdog_fires(
            Instant::now() + NO_SPEECH_TIMEOUT + Duration::from_millis(1)
        ));
        let outcome = machine.watchdog_fire();
        assert!(matches!(
            outcome,
            ReleaseOutcome::Discarded(WATCHDOG_GUIDANCE)
        ));
        assert_eq!(machine.phase(), LivePhase::Idle);
    }

    #[test]
    fn speech_refreshes_watchdog_deadline() {
        let mut machine = LiveMachine::new();
        machine.press();
        machine.audio(&speech_chunk());
        // Even long after the press, recent speech keeps the watchdog at bay.
        assert!(!machine.watchdog_fires(Instant::now() + Duration::from_secs(9)));
    }

    #[test]
    fn abort_drops_capture() {
        let mut machine = LiveMachine::new();
        machine.press();
        machine.audio(&speech_chunk());
        machine.abort();
        assert_eq!(machine.phase(), LivePhase::Idle);
        assert!(matches!(
            machine.release(),
            ReleaseOutcome::Discarded("No active recording")
        ));
    }

    #[test]
    fn bounded_backlog_drops_oldest_samples() {
        let mut buffer = AudioBuffer::default();
        let chunk = speech_chunk();
        for _ in 0..((AudioBuffer::max_samples() / chunk.len()) + 2) {
            buffer.push(&chunk);
        }
        assert!(buffer.len() <= AudioBuffer::max_samples());
        assert_eq!(buffer.duration(), Duration::from_secs_f32(30.0));
    }

    // ------------------------------------------------------------------
    // STT client against a mock HTTP server
    // ------------------------------------------------------------------

    /// Serves one request on a dedicated blocking thread (deterministic in
    /// tests — no tokio task scheduling involved); `assert_request` inspects
    /// the raw HTTP/1.1 request head, path, and body, then replies with
    /// `response`.
    fn serve_once(
        assert_request: impl FnOnce(String, String, Vec<u8>) + Send + 'static,
        response: Vec<u8>,
    ) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            // Phase 1: read the header section (ASCII) up to the terminator.
            let mut request: Vec<u8> = Vec::new();
            let mut buffer = [0u8; 4096];
            let header_end = loop {
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
                let read = socket.read(&mut buffer).expect("read");
                if read == 0 {
                    break 0;
                }
                request.extend_from_slice(&buffer[..read]);
            };
            let head = String::from_utf8_lossy(&request[..header_end]).into_owned();
            let content_length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .unwrap_or(0);
            // Phase 2: read exactly content-length raw body bytes (the WAV is
            // binary; it must never go through a UTF-8 lossy conversion).
            while request.len() < header_end.saturating_add(content_length) {
                let read = socket.read(&mut buffer).expect("read body");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let path = head.split(' ').nth(1).unwrap_or("").to_owned();
            assert_request(head, path, request[header_end..].to_vec());
            socket.write_all(&response).expect("respond");
        });
        format!("http://{address}")
    }

    /// Builds a JSON HTTP/1.1 response with a correct Content-Length (the
    /// literal fixtures were hand-miscounted and mis-framed the response).
    fn http_response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn wav_fixture() -> AudioCapture {
        let mut machine = LiveMachine::new();
        machine.press();
        machine.audio(&speech_chunk());
        machine.audio(&speech_chunk());
        match machine.release() {
            ReleaseOutcome::Capture(capture) => capture,
            ReleaseOutcome::Discarded(_) => panic!("fixture capture"),
        }
    }

    #[tokio::test]
    async fn stt_client_sends_expected_multipart_shape() {
        let base = serve_once(
            |head, path, body| {
                // The boundary must be read from the original-case head: the
                // body preserves the boundary's case, so lowercasing first
                // would break the terminating-boundary match below.
                let boundary = head
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-type: multipart/form-data; boundary=")
                            .map(str::trim)
                    })
                    .expect("multipart content type");
                let head = head.to_ascii_lowercase();
                assert_eq!(path, "/v1/audio/transcriptions", "base URL join");
                assert!(
                    head.contains("authorization: bearer test-live-key"),
                    "bearer header: {head}"
                );
                let body = String::from_utf8_lossy(&body).to_owned();
                assert!(
                    body.contains("name=\"file\"; filename=\"audio.wav\""),
                    "file part: {body}"
                );
                assert!(body.contains("name=\"model\""), "model part: {body}");
                assert!(body.contains("whisper-1"), "model value: {body}");
                assert!(!body.contains("name=\"language\""), "no language when unset");
                assert!(body.contains("RIFF"), "wav bytes embedded: {body}");
                assert!(
                    body.contains(&format!("--{boundary}--")),
                    "terminating boundary; boundary={boundary}; received body: {body}"
                );
            },
            http_response("200 OK", r#"{"text":"hello world"}"#),
        );
        let settings = settings(&base);
        let client = SttClient::new().expect("client");
        let text = client
            .transcribe(&settings, &wav_fixture())
            .await
            .expect("transcribe");
        assert_eq!(text, "hello world");
    }

    #[tokio::test]
    async fn stt_client_includes_language_when_configured() {
        let base = serve_once(
            |_head, _path, body| {
                let body = String::from_utf8_lossy(&body).to_owned();
                assert!(body.contains("name=\"language\""), "language part: {body}");
                assert!(body.contains("en-US"), "language value: {body}");
            },
            http_response("200 OK", r#"{"text":"translated"}"#),
        );
        let mut configured = settings(&base);
        configured.language = Some("en-US".to_owned());
        let client = SttClient::new().expect("client");
        let text = client
            .transcribe(&configured, &wav_fixture())
            .await
            .expect("transcribe");
        assert_eq!(text, "translated");
    }

    #[tokio::test]
    async fn stt_client_surfaces_http_errors_actionably() {
        let base = serve_once(
            |_head, _path, _body| {},
            http_response("401 Unauthorized", r#"{"error":{"message":"invalid api key"}}"#),
        );
        let client = SttClient::new().expect("client");
        let error = client
            .transcribe(&settings(&base), &wav_fixture())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("401"), "{error}");
        // The endpoint URL/port never leaks into the error (the browser must
        // not learn where the STT endpoint lives).
        assert!(!error.contains("127.0.0.1"), "{error}");
        assert!(!error.contains("/v1/audio/transcriptions"), "{error}");
    }

    #[tokio::test]
    async fn stt_client_transport_failure_never_echoes_endpoint() {
        // A connection failure against a closed port: the error is the fixed
        // "STT request failed" — no base URL, port, or path.
        let base = "http://127.0.0.1:1";
        let client = SttClient::new().expect("client");
        let error = client
            .transcribe(&settings(base), &wav_fixture())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("STT request failed"), "{error}");
        assert!(!error.contains("127.0.0.1"), "{error}");
        assert!(!error.contains(":1"), "{error}");
        assert!(!error.contains("/v1/audio/transcriptions"), "{error}");
        assert!(!error.contains("Bearer"), "{error}");
    }

    #[tokio::test]
    async fn stt_client_redacts_secrets_echoed_by_server() {
        let secret = ["s", "k-", "echoed-live-key-1234567890abcdef"].concat();
        let json = format!(r#"{{"error":{{"message":"bad key {secret}"}}}}"#);
        let response = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{json}",
            json.len()
        );
        let base = serve_once(|_head, _path, _body| {}, response.into_bytes());
        let client = SttClient::new().expect("client");
        let error = client
            .transcribe(&settings(&base), &wav_fixture())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !error.contains(secret.as_str()),
            "server-echoed key must be redacted: {error}"
        );
        assert!(error.contains("[REDACTED]"), "redaction marker: {error}");
    }

    #[tokio::test]
    async fn stt_client_rejects_http_endpoint_when_insecure_disallowed() {
        let settings = settings("http://127.0.0.1:1");
        let settings = LiveRuntimeSettings {
            allow_insecure: false,
            ..settings
        };
        let client = SttClient::new().expect("client");
        let error = client
            .transcribe(&settings, &wav_fixture())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("allowInsecure"), "{error}");
    }

    #[tokio::test]
    async fn stt_client_bounds_and_redacts_long_error_bodies() {
        let secret = ["s", "k-", "echoed-live-key-1234567890abcdef"].concat();
        // A long non-2xx body whose secret straddles the snippet boundary:
        // the error must carry at most STT_ERROR_SNIPPET_CHARS chars of body
        // and the key must be scrubbed (redaction before truncation).
        let long = format!("{} {secret} {}", "x".repeat(200), "y".repeat(500)) + &"z".repeat(1200);
        let response = format!(
            "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{long}",
            long.len()
        );
        let base = serve_once(|_head, _path, _body| {}, response.into_bytes());
        let client = SttClient::new().expect("client");
        let error = client
            .transcribe(&settings(&base), &wav_fixture())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("502"), "{error}");
        assert!(!error.contains(secret.as_str()), "key leaked: {error}");
        assert!(error.contains("[REDACTED]"), "redaction marker: {error}");
        assert!(!error.contains("127.0.0.1"), "endpoint leaked: {error}");
        assert!(!error.contains("/v1/audio/transcriptions"), "endpoint path leaked: {error}");
        // The echoed body snippet is bounded by the shared constant.
        let snippet = error
            .split("Bad Gateway: ")
            .nth(1)
            .map(|part| part.chars().count())
            .unwrap_or(usize::MAX);
        assert!(snippet <= STT_ERROR_SNIPPET_CHARS, "{error} ({snippet} chars)");
    }

    #[tokio::test]
    async fn stt_client_bounds_non_json_error_body() {
        // A long non-JSON success body must be bounded in the parse error.
        let long = "not json ".repeat(200);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{long}",
            long.len()
        );
        let base = serve_once(|_head, _path, _body| {}, response.into_bytes());
        let client = SttClient::new().expect("client");
        let error = client
            .transcribe(&settings(&base), &wav_fixture())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("non-JSON"), "{error}");
        assert!(!error.contains("127.0.0.1"), "endpoint leaked: {error}");
        assert!(!error.contains("/v1/audio/transcriptions"), "endpoint path leaked: {error}");
        let snippet = error
            .split("non-JSON response: ")
            .nth(1)
            .map(|part| part.chars().count())
            .unwrap_or(usize::MAX);
        assert!(snippet <= STT_ERROR_SNIPPET_CHARS, "{error} ({snippet} chars)");
    }

    #[tokio::test]
    async fn stt_client_bounds_oversized_response_by_content_length() {
        // A Content-Length declared over the cap fails BEFORE any body read.
        let long = "x".repeat(STT_MAX_RESPONSE_BYTES + 4096);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{long}",
            long.len()
        );
        let base = serve_once(|_head, _path, _body| {}, response.into_bytes());
        let client = SttClient::new().expect("client");
        let error = client
            .transcribe(&settings(&base), &wav_fixture())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds the"), "{error}");
        assert!(error.contains(&STT_MAX_RESPONSE_BYTES.to_string()), "{error}");
        assert!(!error.contains("Bearer"), "{error}");
        assert!(!error.contains("127.0.0.1"), "endpoint leaked: {error}");
    }

    #[tokio::test]
    async fn stt_client_bounds_oversized_response_without_content_length() {
        // Without a Content-Length the body is streamed with the byte cap;
        // an oversized body trips the chunked bound instead of allocating.
        let long = "y".repeat(STT_MAX_RESPONSE_BYTES + 4096);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{long}"
        );
        let base = serve_once(|_head, _path, _body| {}, response.into_bytes());
        let client = SttClient::new().expect("client");
        let error = client
            .transcribe(&settings(&base), &wav_fixture())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds the"), "{error}");
        assert!(!error.contains("Bearer"), "{error}");
        assert!(!error.contains("127.0.0.1"), "endpoint leaked: {error}");
    }

    #[tokio::test]
    async fn stt_client_caps_extracted_transcript_chars() {
        // A success body whose `text` exceeds the transcript char cap is
        // truncated to the bound for the RPC/DOM — never passed through whole.
        let text = "t".repeat(STT_MAX_TRANSCRIPT_CHARS + 1000);
        let body = format!(r#"{{"text":"{text}"}}"#);
        assert!(body.len() <= STT_MAX_RESPONSE_BYTES, "fixture must fit the body cap");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let base = serve_once(|_head, _path, _body| {}, response.into_bytes());
        let client = SttClient::new().expect("client");
        let transcript = client
            .transcribe(&settings(&base), &wav_fixture())
            .await
            .expect("transcribe ok");
        assert_eq!(transcript.chars().count(), STT_MAX_TRANSCRIPT_CHARS, "{transcript:?}");
    }

    #[test]
    fn validate_live_settings_rejects_secret_bearing_base_urls() {
        let mut s = settings("https://stt.example");
        // Userinfo/query/fragment shapes are rejected outright with a fixed
        // message; the raw URL (and its embedded secrets) never appears.
        for url in [
            "https://user:pa%24sword@stt.example",
            "https://stt.example/v1?token=abc123",
            "https://stt.example/v1#frag",
            "https://user@stt.example",
        ] {
            s.stt_base_url = url.to_owned();
            let error = validate_live_settings(&s).unwrap_err().to_string();
            assert!(!error.contains(url), "raw URL echoed: {url} -> {error}");
            assert!(error.contains("Settings.live.sttBaseUrl"), "{error}");
        }
        // An unparseable URL fails with a fixed message, no echo.
        s.stt_base_url = "not a url".to_owned();
        let error = validate_live_settings(&s).unwrap_err().to_string();
        assert!(!error.contains("not a url"), "raw URL echoed: {error}");
        assert!(error.contains("Settings.live.sttBaseUrl"), "{error}");

        // A bare base URL still validates (http allowed via allowInsecure).
        s.stt_base_url = "https://stt.example".to_owned();
        assert!(validate_live_settings(&s).is_ok());
        s.stt_base_url = "http://127.0.0.1:9000".to_owned();
        assert!(validate_live_settings(&s).is_ok());
        // Plaintext without allowInsecure: fixed message, no raw URL echo.
        s.allow_insecure = false;
        let error = validate_live_settings(&s).unwrap_err().to_string();
        assert!(error.contains("allowInsecure"), "{error}");
        assert!(!error.contains("127.0.0.1"), "raw URL echoed: {error}");
    }

    // ------------------------------------------------------------------
    // Session pipeline (fake capture backend)
    // ------------------------------------------------------------------

    struct ScriptedCapture {
        chunks: VecDeque<Vec<i16>>,
    }

    #[async_trait::async_trait]
    impl CaptureBackend for ScriptedCapture {
        async fn next_chunk(&mut self) -> Result<Option<Vec<i16>>> {
            if let Some(chunk) = self.chunks.pop_front() {
                return Ok(Some(chunk));
            }
            future::pending().await
        }
    }

    #[tokio::test]
    async fn session_press_release_transcribes_to_draft_event() {
        // Mock STT server answering the session's transcribe call.
        let base = serve_once(
            |_head, _path, _body| {},
            http_response("200 OK", r#"{"text":"press release"}"#),
        );
        let settings = settings(&base);
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let stt = SttClient::new().expect("client");
        let mut chunks = VecDeque::new();
        chunks.push_back(speech_chunk());
        chunks.push_back(speech_chunk());
        let backend: Box<dyn CaptureBackend> = Box::new(ScriptedCapture { chunks });
        let task = tokio::spawn(run_ptt_session(
            settings,
            stt,
            backend,
            control_rx,
            event_tx,
            NO_SPEECH_TIMEOUT,
        ));
        let first = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("first event");
        assert_eq!(first, Some(PttSessionEvent::Started));
        control_tx.send(PttControl::Release).expect("release");
        let terminal = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("terminal event");
        assert_eq!(
            terminal,
            Some(PttSessionEvent::Transcript {
                text: "press release".to_owned()
            })
        );
        task.await.expect("session task finished");
    }

    #[tokio::test]
    async fn session_watchdog_stops_silent_capture_with_guidance() {
        let mut chunks = VecDeque::new();
        chunks.push_back(silence_chunk());
        let backend: Box<dyn CaptureBackend> = Box::new(ScriptedCapture { chunks });
        let settings = settings("https://stt.invalid");
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let stt = SttClient::new().expect("client");
        let task = tokio::spawn(run_ptt_session(
            settings,
            stt,
            backend,
            control_rx,
            event_tx,
            Duration::from_millis(50),
        ));
        let first = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("started");
        assert_eq!(first, Some(PttSessionEvent::Started));
        let terminal = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("watchdog event");
        assert_eq!(
            terminal,
            Some(PttSessionEvent::Stopped {
                reason: WATCHDOG_GUIDANCE.to_owned()
            })
        );
        drop(control_tx);
        task.await.expect("session task finished");
    }

    #[tokio::test]
    async fn session_abort_drops_capture_without_transcript() {
        let mut chunks = VecDeque::new();
        chunks.push_back(speech_chunk());
        let backend: Box<dyn CaptureBackend> = Box::new(ScriptedCapture { chunks });
        let settings = settings("https://stt.invalid");
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let stt = SttClient::new().expect("client");
        let task = tokio::spawn(run_ptt_session(
            settings,
            stt,
            backend,
            control_rx,
            event_tx,
            NO_SPEECH_TIMEOUT,
        ));
        let first = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("started");
        assert_eq!(first, Some(PttSessionEvent::Started));
        control_tx.send(PttControl::Abort).expect("abort");
        let terminal = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("abort event");
        assert_eq!(
            terminal,
            Some(PttSessionEvent::Stopped {
                reason: "Aborted".to_owned()
            })
        );
        task.await.expect("session task finished");
    }

    #[test]
    fn redact_secrets_scrubs_stt_error_text() {
        let secret = ["s", "k-", "echoed-live-key-1234567890abcdef"].concat();
        let redacted = redact_secrets(&format!("STT endpoint error: bad key {secret}"));
        assert!(!redacted.contains("sk-echoed"), "{redacted}");
        assert!(redacted.contains("[REDACTED]"));
    }

    // ------------------------------------------------------------------
    // Delegation candidate detection (client-side delegation.created)
    // ------------------------------------------------------------------

    #[test]
    fn delegation_detection_flags_coding_tasks() {
        for text in [
            "implement the fetch function in api.rs",
            "fix the bug in parser.rs",
            "add tests for the tokenizer",
            "refactor the module into smaller files",
            "write a unit test for the http client",
            "update the config in settings.toml",
            "debug the crash in src/main",
            "remove the dead code in lib.rs",
            "create a function that validates the input",
            "migrate the database schema",
            "Implement the Fetch Function in Api.RS",
            "PLEASE fix src/coding/live.rs",
        ] {
            assert!(is_delegation_candidate(text), "candidate: {text}");
        }
    }

    #[test]
    fn delegation_detection_ignores_plain_speech() {
        for text in [
            "",
            "   ",
            "hello world",
            "how are you",
            "what time is it",
            "tell me a joke",
            "thanks for your help",
            "fix my car please",
            "call the doctor",
            "the weather is nice today",
            "test the microphone",
        ] {
            assert!(!is_delegation_candidate(text), "plain: {text}");
        }
    }

    #[test]
    fn delegation_detection_requires_verb_and_code_signal() {
        // A signal without a verb is not a task.
        assert!(!is_delegation_candidate("parser.rs"));
        assert!(!is_delegation_candidate("src/main.rs"));
        // A verb without a code-domain signal is not a coding task.
        assert!(!is_delegation_candidate("fix"));
        assert!(!is_delegation_candidate("implement"));
        assert!(!is_delegation_candidate("fix the kitchen sink"));
        // Verbs must match as whole words, not substrings.
        assert!(!is_delegation_candidate("the unfixed draft"), "no whole-word verb");
        assert!(
            is_delegation_candidate("fix the module"),
            "code word carries the signal"
        );
        assert!(
            is_delegation_candidate("add tests"),
            "verb 'add' + plural code word 'tests'"
        );
    }
}
