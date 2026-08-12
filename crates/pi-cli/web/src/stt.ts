/**
 * STT (hold-to-talk) pure helpers — the bounded WAV encode/resample and
 * size guard shared by App.tsx's transcribeAudio flow and the node-runnable
 * regression test (scripts/stt.test.ts). DELIBERATELY has no DOM/browser
 * dependency so `esbuild --platform=node` can bundle the test in isolation
 * (mirroring src/realtime.ts + scripts/realtime.test.ts).
 *
 * The browser sends ONLY the bounded audio and a fixed MIME type over the
 * `stt_transcribe` RPC; the endpoint URL and API key never leave the backend.
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
