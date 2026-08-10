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

use crate::redact::redact_secrets;
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
/// Ten-second no-speech watchdog (Hyper parity): capture stops with guidance
/// instead of leaving a dead mic streaming indefinitely.
pub const NO_SPEECH_TIMEOUT: Duration = Duration::from_secs(10);
/// Minimum utterance length before release is worth transcribing.
pub const MIN_UTTERANCE: Duration = Duration::from_millis(250);
/// Peak sample amplitude (of 32767) treated as speech for the watchdog.
pub const SPEECH_PEAK_THRESHOLD: i16 = 300;
/// HTTP client timeout for the STT request.
pub const STT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Guidance text emitted when the no-speech watchdog stops a capture.
pub const WATCHDOG_GUIDANCE: &str = "No speech detected — release and hold again to talk";

/// Multipart filename sent as `file=audio.wav` (the WAV is encoded in
/// memory; nothing touches disk).
pub const AUDIO_FILENAME: &str = "audio.wav";

// ---------------------------------------------------------------------------
// Settings validation
// ---------------------------------------------------------------------------

/// Validates that `settings` can drive a `/live` session. Returns actionable
/// errors — each names the exact `Settings.live.*` key to fix — for the
/// disabled, unconfigured, and plaintext-endpoint cases.
pub fn validate_live_settings(settings: &LiveRuntimeSettings) -> Result<()> {
    if !settings.enabled {
        bail!(
            "Live voice is disabled — set `Settings.live.enabled = true` (or run `/settings set live.enabled true`)"
        );
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
    let parsed = url::Url::parse(settings.stt_base_url.trim())
        .with_context(|| format!("`Settings.live.sttBaseUrl` is not a valid URL: {}", settings.stt_base_url))?;
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
        let response = self
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
            .with_context(|| format!("STT request to {url} failed"))?;
        let status = response.status();
        let bytes = response.bytes().await.context("reading STT response body")?;
        let text = String::from_utf8_lossy(&bytes);
        if !status.is_success() {
            // Redact credential shapes the server may echo back (e.g. the
            // bearer token inside a proxy error body).
            return Err(anyhow!(
                "STT endpoint {url} returned {status}: {}",
                redact_secrets(&text)
            ));
        }
        parse_transcript_response(&text)
    }
}

/// Extracts `text` from an OpenAI-compatible transcriptions response
/// (`{"text": "..."}`). A server-side `error` field becomes an actionable
/// error; the text is trimmed and returned verbatim otherwise.
fn parse_transcript_response(body: &str) -> Result<String> {
    let value: Value = serde_json::from_str(body).with_context(|| {
        format!(
            "STT endpoint returned non-JSON response: {}",
            redact_secrets(body)
        )
    })?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown STT error");
        return Err(anyhow!("STT endpoint error: {}", redact_secrets(message)));
    }
    value
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow!(
                "STT endpoint response had no `text` field: {}",
                redact_secrets(body)
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
        assert!(error.contains("/v1/audio/transcriptions"), "{error}");
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
