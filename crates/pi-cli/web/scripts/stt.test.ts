#!/usr/bin/env node
// Focused STT (hold-to-talk) pure regression for src/stt.ts — the bounded
// WAV encode + 48k→16k resample and the decoded-size guard shared by
// App.tsx's transcribeAudio flow. Run through `npm run build`, which bundles
// this file with esbuild into a disposable Node module before executing the
// assertions.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import {
  STT_AUTO_RELEASE_MS,
  STT_MAX_WAV_BYTES,
  STT_SAMPLE_RATE,
  STT_WAV_MIME,
  encodeWavPcm16,
  resampleToSttRate,
  wavToBase64,
} from '../src/stt.ts';
const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- wavToBase64: exact base64 round-trip + cap guard ----
{
  const wav = encodeWavPcm16(new Float32Array(4), STT_SAMPLE_RATE);
  const encoded = wavToBase64(wav);
  check('in-cap WAV encodes to non-empty base64', typeof encoded === 'string' && encoded.length > 0);
  const decoded = Buffer.from(encoded as string, 'base64');
  check(
    'base64 round-trips the exact bytes',
    decoded.length === wav.length && decoded.every((byte, i) => byte === wav[i])
  );
  check('mime allowlist constant is audio/wav', STT_WAV_MIME === 'audio/wav');
  check(
    'decoded cap is 44 + 30 s of 16 kHz mono PCM16',
    STT_MAX_WAV_BYTES === 44 + 16000 * 2 * 30
  );

  // The UI auto-release sits below the backend 30 s cap with a safety
  // margin (a late setTimeout must never produce a capture the strict
  // backend parser would reject). Nominal (no lateness) is strictly under;
  // 500 ms late lands exactly AT the cap (960044 bytes), which both the
  // client guard and the backend accept (`>` rejects; at-cap passes).
  check(
    'auto-release sits below the 30 s cap with margin',
    STT_AUTO_RELEASE_MS > 29000 && STT_AUTO_RELEASE_MS < 30000
  );
  check(
    'nominal auto-release capture is strictly under the byte cap',
    (STT_AUTO_RELEASE_MS / 1000) * 16000 * 2 + 44 < STT_MAX_WAV_BYTES
  );
  check(
    'auto-release +500 ms lateness never exceeds the backend byte cap',
    ((STT_AUTO_RELEASE_MS + 500) / 1000) * 16000 * 2 + 44 <= STT_MAX_WAV_BYTES
  );

  // Exactly at the cap is accepted; one byte over returns null (the caller
  // surfaces the bounded error instead of sending).
  const atCap = new Uint8Array(STT_MAX_WAV_BYTES);
  check('exactly at the cap is accepted', wavToBase64(atCap) !== null);
  const overCap = new Uint8Array(STT_MAX_WAV_BYTES + 1);
  check('one byte over the cap returns null', wavToBase64(overCap) === null);

  // Empty input encodes to the empty string (the backend rejects the
  // non-WAV shape with a bounded error).
  check('empty input encodes to empty string', wavToBase64(new Uint8Array(0)) === '');
}

// ---- resampleToSttRate: 48 kHz capture -> fixed 16 kHz contract ----
{
  check('STT_SAMPLE_RATE is 16000', STT_SAMPLE_RATE === 16000);
  const oneSec48k = new Float32Array(48000).fill(0.25);
  const resampled = resampleToSttRate(oneSec48k, 48000);
  check('48k -> 16k yields 16000 samples for 1 s', resampled.length === 16000);
  check('constant input resamples to the same value', resampled.every((v) => Math.abs(v - 0.25) < 1e-6));
  // Endpoint samples: out[0] equals input[0]; integer positions map exactly
  // at the 1/3 ratio.
  const ramp = new Float32Array(48000).map((_, i) => i);
  const out = resampleToSttRate(ramp, 48000);
  check('out[0] equals input[0]', out[0] === ramp[0]);
  check('integer-position samples map exactly', out[3] === ramp[9] && out[16000 - 1] === ramp[48000 - 3]);
  // 16k in -> 16k out (identity length).
  check('16k -> 16k keeps the length', resampleToSttRate(ramp.subarray(0, 16000), 16000).length === 16000);
  // Degenerate rates fail to an empty array (caller surfaces the error).
  check('zero rate yields an empty array', resampleToSttRate(oneSec48k, 0).length === 0);
}

// ---- encodeWavPcm16: header fixed at the STT rate + 30 s cap ----
{
  // The App ALWAYS passes STT_SAMPLE_RATE (blobToWav resamples first); the
  // encoder is called with the fixed rate, so the header must carry it.
  const wav = encodeWavPcm16(new Float32Array(32000).fill(0), STT_SAMPLE_RATE);
  const view = new DataView(wav.buffer);
  check(
    'header sample rate is the STT rate (16000)',
    view.getUint32(24, true) === STT_SAMPLE_RATE && view.getUint32(24, true) === 16000
  );
  check('header declares PCM16 mono', view.getUint16(20, true) === 1 && view.getUint16(22, true) === 1 && view.getUint16(34, true) === 16);
  check('canonical 44-byte header length', wav.length === 44 + 32000 * 2 && view.getUint32(40, true) === 32000 * 2);

  // 30 s of 48 kHz capture resamples to exactly 30 s at 16 kHz — the byte
  // cap matches the 30-second contract for ANY capture rate.
  const thirtySec48k = resampleToSttRate(new Float32Array(48000 * 30), 48000);
  check('30 s at 48 kHz resamples to exactly 30 s at 16 kHz', thirtySec48k.length === 16000 * 30);
  const wav30 = encodeWavPcm16(thirtySec48k, STT_SAMPLE_RATE);
  check('30 s WAV is exactly 44 + 960000 bytes (== cap)', wav30.length === STT_MAX_WAV_BYTES);
  check('30 s WAV passes the cap guard', wavToBase64(wav30) !== null);

  // 31 s at 48 kHz exceeds the cap after resampling -> null (bounded error).
  const thirtyOneSec48k = resampleToSttRate(new Float32Array(48000 * 31), 48000);
  const wav31 = encodeWavPcm16(thirtyOneSec48k, STT_SAMPLE_RATE);
  check('31 s WAV exceeds the cap -> null', wavToBase64(wav31) === null);
}

if (failures.length > 0) {
  console.error('stt.test.ts FAILED:\n' + failures.join('\n'));
  process.exit(1);
}
console.log(`stt.test.ts PASSED (${ran} checks)`);
