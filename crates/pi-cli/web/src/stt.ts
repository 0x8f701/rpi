/**
 * STT (hold-to-talk) pure helpers — the bounded WAV encode/resample, the
 * decoded-size guard, and the captured-session owner check shared by
 * App.tsx's transcribeAudio flow and the node-runnable regression test
 * (scripts/stt.test.ts). DELIBERATELY has no DOM/browser dependency so
 * `esbuild --platform=node` can bundle the test in isolation (mirroring
 * src/realtime.ts + scripts/realtime.test.ts).
 *
 * The browser sends ONLY the bounded audio and a fixed MIME type over the
 * `stt_transcribe` RPC; the endpoint URL and API key never leave the backend.
 * The RPC and any composer write stay bound to the session captured when
 * transcription begins — a late result after an A→B switch is dropped
 * instead of mutating B's composer.
 */

/** Backend-side decoded-size cap for browser-recorded STT audio: a 44-byte
 *  RIFF/WAVE header plus 30 seconds of 16 kHz mono 16-bit PCM
 *  (44 + 16_000 * 2 * 30). Mirrors the Rust `stt_transcribe` bound so the
 *  browser rejects oversize captures before they cross the wire. */
export const STT_MAX_WAV_BYTES = 44 + 16000 * 2 * 30;

/** The only MIME type the backend `stt_transcribe` RPC accepts. The browser
 *  converts the MediaRecorder container to WAV first (blobToWav), so the raw
 *  webm/mp4 capture never crosses the wire. */
export const STT_WAV_MIME = 'audio/wav';

/** Fixed WAV sample rate for the STT contract (16 kHz mono PCM16), matching
 *  the backend's decoded-size cap: 30 seconds of 16 kHz mono PCM16 is
 *  960,000 bytes. The browser's AudioBuffer decode commonly yields 48 kHz,
 *  so captures are resampled to this rate before encoding — otherwise a
 *  48 kHz recording would hit the byte cap at ~10 seconds and the 30-second
 *  hold contract could never be met. */
export const STT_SAMPLE_RATE = 16000;

/** Hold-to-talk recording AUTO-RELEASE (ms). A safety margin BELOW the
 *  backend's 30-second decoded-size cap: the `setTimeout` can fire late
 *  under scheduler pressure, and a capture longer than 30 s would exceed
 *  the byte cap and be rejected by the strict backend parser. The backend
 *  cap itself stays exactly 30 s; the UI releases just before it. */
export const STT_AUTO_RELEASE_MS = 29500;

/** Linearly resample mono float32 PCM samples (AudioBuffer channel 0) from
 *  `inputRate` Hz to [`STT_SAMPLE_RATE`]. Linear interpolation is the
 *  standard lightweight voice resampler. Returns an empty array for a
 *  degenerate rate (callers surface the conversion error path). */
export function resampleToSttRate(input: Float32Array, inputRate: number): Float32Array {
  if (inputRate <= 0 || !Number.isFinite(inputRate) || input.length === 0) {
    return new Float32Array(0);
  }
  const ratio = STT_SAMPLE_RATE / inputRate; // output samples per input sample
  const outLen = Math.max(1, Math.floor(input.length * ratio));
  const out = new Float32Array(outLen);
  for (let i = 0; i < outLen; i++) {
    const pos = i / ratio; // fractional input position
    const i0 = Math.floor(pos);
    const i1 = Math.min(input.length - 1, i0 + 1);
    const frac = pos - i0;
    out[i] = input[i0] * (1 - frac) + input[i1] * frac;
  }
  return out;
}

/** Encode mono float32 PCM samples as a canonical RIFF/WAVE PCM16 buffer at
 *  `sampleRate`. The header is written exactly like the canonical 44-byte
 *  layout; samples are clamped to [-1, 1] and converted to signed 16-bit.
 *  Callers pass [`STT_SAMPLE_RATE`] so the wire contract rate is fixed. The
 *  return is explicitly `Uint8Array<ArrayBuffer>` (never `ArrayBufferLike`,
 *  which TS 5.7 refuses as a `BlobPart` for `Blob` construction). */
export function encodeWavPcm16(samples: Float32Array, sampleRate: number): Uint8Array<ArrayBuffer> {
  const numChannels = 1;
  const bytesPerSample = 2;
  const blockAlign = numChannels * bytesPerSample;
  const dataSize = samples.length * blockAlign;
  const out = new Uint8Array(44 + dataSize);
  const view = new DataView(out.buffer);
  out.set([0x52, 0x49, 0x46, 0x46], 0); // "RIFF"
  view.setUint32(4, 36 + dataSize, true);
  out.set([0x57, 0x41, 0x56, 0x45], 8); // "WAVE"
  out.set([0x66, 0x6d, 0x74, 0x20], 12); // "fmt "
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true); // PCM
  view.setUint16(22, numChannels, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * blockAlign, true);
  view.setUint16(32, blockAlign, true);
  view.setUint16(34, 16, true);
  out.set([0x64, 0x61, 0x74, 0x61], 36); // "data"
  view.setUint32(40, dataSize, true);
  let offset = 44;
  for (let i = 0; i < samples.length; i++) {
    const s = Math.max(-1, Math.min(1, samples[i]));
    view.setInt16(offset, s < 0 ? s * 0x8000 : s * 0x7fff, true);
    offset += 2;
  }
  return out;
}

/** Encode WAV bytes as standard base64 for the `stt_transcribe` RPC payload.
 *  Returns null when the payload would exceed the backend's decoded-size cap
 *  (the caller surfaces the bounded error instead of sending). Chunked
 *  String.fromCharCode keeps the call stack flat for ~1 MiB buffers. */
export function wavToBase64(bytes: Uint8Array): string | null {
  if (bytes.length > STT_MAX_WAV_BYTES) return null;
  let binary = '';
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    binary += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return btoa(binary);
}

/** `stt_transcribe` RPC targeted at the session captured when transcription
 *  began. Stamping `sessionId` here means a late result after an A→B switch
 *  still routes to A's runtime instead of falling back to the active session. */
export function sttTranscribeCommand(
  owner: string,
  audioBase64: string,
  mimeType: string,
): { type: 'stt_transcribe'; audioBase64: string; mimeType: string; sessionId: string } {
  return { type: 'stt_transcribe', audioBase64, mimeType, sessionId: owner };
}

/** True when a captured STT owner may mutate the active composer. A late
 *  transcript is dropped unless the owner is a non-empty string that still
 *  matches the current session — switching A→B must never write A's result
 *  into B's draft. */
export function shouldCommitSttTranscript(owner: string | null | undefined, current: string | null | undefined): boolean {
  return typeof owner === 'string' && owner !== '' && owner === current;
}

/** Submit kind for a committed STT transcript against the OWNER's state at
 *  result time (owner running -> steer, else prompt). App.tsx consumes this;
 *  the node test drives the same decision table through the seam. */
export function planSttCommit(
  owner: string | null | undefined,
  current: string | null | undefined,
  streaming: boolean,
): 'steer' | 'prompt' | null {
  if (!shouldCommitSttTranscript(owner, current)) return null;
  return streaming ? 'steer' : 'prompt';
}

/** Hold-to-talk capture may begin only after the listener has bound a concrete
 * session owner. Empty ownership must fail before getUserMedia; otherwise the
 * later RPC routing fallback can retarget the recording to another session. */
export function canStartSttRecording(owner: string | null | undefined): owner is string {
  return typeof owner === 'string' && owner !== '';
}

export const STT_RECORDER_MIME_CANDIDATES = ['audio/webm;codecs=opus', 'audio/webm', 'audio/mp4'] as const;

export const STT_UNCONFIGURED_MESSAGE =
  'Live voice is not configured — set Settings.live.sttBaseUrl and sttApiKey (and sttModel) in the terminal';
export const STT_HTTPS_MESSAGE =
  'microphone requires HTTPS (or localhost). Use https:// or connect via 127.0.0.1.';
export const STT_NO_SESSION_MESSAGE = 'voice recording requires an active session';
export const STT_WAV_CONVERT_MESSAGE = 'transcription failed: recording could not be converted to WAV';
export const STT_EMPTY_TRANSCRIPT_MESSAGE = 'transcription failed: no transcript returned';

export function sttWavCapMessage(): string {
  return `transcription failed: recording exceeds the ${STT_MAX_WAV_BYTES}-byte WAV cap`;
}

export function sttMicrophoneUnavailableMessage(err: unknown): string {
  return `microphone unavailable: ${err instanceof Error ? err.message : String(err)}`;
}

export function sttTranscribeFailedMessage(err: unknown): string {
  return `transcription failed: ${err instanceof Error ? err.message : String(err)}`;
}

export function selectSttRecorderMime(
  isTypeSupported: ((mime: string) => boolean) | null | undefined,
): string | undefined {
  if (typeof isTypeSupported !== 'function') return undefined;
  for (const mime of STT_RECORDER_MIME_CANDIDATES) {
    if (isTypeSupported(mime)) return mime;
  }
  return undefined;
}

export type SttStartPlan =
  | { ok: true; owner: string }
  | { ok: false; message: string };

export function planSttStart(args: {
  owner: string | null | undefined;
  live: { sttConfigured?: boolean } | null;
  hasGetUserMedia: boolean;
}): SttStartPlan {
  if (args.live != null && args.live.sttConfigured !== true) {
    return { ok: false, message: STT_UNCONFIGURED_MESSAGE };
  }
  if (!args.hasGetUserMedia) {
    return { ok: false, message: STT_HTTPS_MESSAGE };
  }
  if (!canStartSttRecording(args.owner)) {
    return { ok: false, message: STT_NO_SESSION_MESSAGE };
  }
  return { ok: true, owner: args.owner };
}

export function sttTranscriptText(raw: unknown): string {
  if (raw && typeof raw === 'object' && typeof (raw as { text?: unknown }).text === 'string') {
    return (raw as { text: string }).text;
  }
  return '';
}

export type SttTranscribeResult =
  | { kind: 'drop' }
  | { kind: 'error'; message: string }
  | { kind: 'transcript'; text: string };

export function planSttTranscribeResult(args: {
  owner: string | null | undefined;
  current: string | null | undefined;
  wavOk: boolean;
  audioBase64: string | null;
  text: string;
}): SttTranscribeResult {
  if (!args.wavOk) {
    return shouldCommitSttTranscript(args.owner, args.current)
      ? { kind: 'error', message: STT_WAV_CONVERT_MESSAGE }
      : { kind: 'drop' };
  }
  if (args.audioBase64 === null) {
    return shouldCommitSttTranscript(args.owner, args.current)
      ? { kind: 'error', message: sttWavCapMessage() }
      : { kind: 'drop' };
  }
  if (!shouldCommitSttTranscript(args.owner, args.current)) return { kind: 'drop' };
  if (!args.text) return { kind: 'error', message: STT_EMPTY_TRANSCRIPT_MESSAGE };
  return { kind: 'transcript', text: args.text };
}

/** Convert a MediaRecorder blob to 16 kHz WAV; null on an undecodable container. */
export async function blobToWav(blob: Blob): Promise<Blob | null> {
  try {
    const webkitWindow = globalThis as unknown as {
      AudioContext?: typeof AudioContext;
      webkitAudioContext?: typeof AudioContext;
    };
    const AudioCtor = webkitWindow.AudioContext ?? webkitWindow.webkitAudioContext;
    if (!AudioCtor) return null;
    const ctx = new AudioCtor();
    try {
      const audioBuffer = await ctx.decodeAudioData(await blob.arrayBuffer());
      const channel = audioBuffer.numberOfChannels > 0 ? audioBuffer.getChannelData(0) : new Float32Array(0);
      const resampled = resampleToSttRate(channel, Math.max(1, Math.round(audioBuffer.sampleRate)));
      return new Blob([encodeWavPcm16(resampled, STT_SAMPLE_RATE)], { type: STT_WAV_MIME });
    } finally {
      void ctx.close();
    }
  } catch {
    return null;
  }
}
