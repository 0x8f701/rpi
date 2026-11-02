#!/usr/bin/env node
// Focused STT (hold-to-talk) pure regression for src/stt.ts — the bounded
// WAV encode + 48k→16k resample, the decoded-size guard, and the captured-
// session owner check shared by App.tsx's transcribeAudio flow. Run through
// `npm run build`, which bundles this file with esbuild into a disposable
// Node module before executing the assertions.
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
  sttTranscribeCommand,
  shouldCommitSttTranscript,
  canStartSttRecording,
  planSttCommit,
  planSttStart,
  planSttTranscribeResult,
  selectSttRecorderMime,
  sttTranscriptText,
  STT_EMPTY_TRANSCRIPT_MESSAGE,
  STT_HTTPS_MESSAGE,
  STT_NO_SESSION_MESSAGE,
  STT_UNCONFIGURED_MESSAGE,
  STT_WAV_CONVERT_MESSAGE,
  sttWavCapMessage,
} from '../src/stt.ts';
import {
  captureSttRecordingDestination,
  planSttTranscriptRoute,
} from '../src/useSttRecorder.ts';
import { compactSendAdmission } from '../src/compactProgress.ts';
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

// ---- captured owner: A start -> switch B -> late STT result ----
{
  // Transcription begins on A; the owner is captured then. A later switch
  // to B must not retarget the RPC or write the late transcript into B's
  // composer. Prior code read sessionIdRef.current at result time, so the
  // RPC and composer write both landed on B.
  const owner = 'A';
  let current = 'A';
  current = 'B';
  const cmd = sttTranscribeCommand(owner, 'ZGF0YQ==', STT_WAV_MIME);
  check('late STT RPC type is stt_transcribe', cmd.type === 'stt_transcribe');
  check('A start -> switch B: late STT RPC stays on A', cmd.sessionId === 'A');
  check('A start -> switch B: late STT RPC is not B', cmd.sessionId !== current);
  check('A start -> switch B: late STT does not write B composer', shouldCommitSttTranscript(owner, current) === false);
  check('same-session STT still commits', shouldCommitSttTranscript(owner, 'A') === true);
  check('empty owner never commits', shouldCommitSttTranscript('', 'A') === false);
  check('null owner never commits', shouldCommitSttTranscript(null, 'A') === false);
  check('null current never commits', shouldCommitSttTranscript('A', null) === false);
  check('bound owner may start STT capture', canStartSttRecording('A') === true);
  check('null owner rejects STT before getUserMedia', canStartSttRecording(null) === false);
  check('empty owner rejects STT before routing fallback', canStartSttRecording('') === false);
  check('fixed late STT command mime is the WAV allowlist', cmd.mimeType === STT_WAV_MIME);

}

// ---- post-transcription dispatch: owner state decides steer vs prompt ----
// App.tsx reads the captured owner's live streaming mirror at result time through this seam.
{
  check('idle->running between press and result => steer', planSttCommit('A', 'A', true) === 'steer');
  check('running->idle between press and result => prompt', planSttCommit('A', 'A', false) === 'prompt');
  check('switched-away owner (running) never dispatches', planSttCommit('A', 'B', true) === null);
  check('switched-away owner (idle) never dispatches', planSttCommit('A', 'B', false) === null);
  check('empty owner never dispatches', planSttCommit('', 'A', true) === null);
  check('null owner never dispatches', planSttCommit(null, 'A', false) === null);
}

// ---- compact-mode dispatch: gated prompt keeps the transcript draft ----
// transcribeAudio appends the transcript BEFORE dispatching; submit consults
// the compact send gate first (prompt only) and rejects without clearing.
{
  const sendWithGate = (value: string, kind: 'steer' | 'prompt', compacting: boolean): { value: string; sent: boolean } => {
    if (kind === 'prompt' && compactSendAdmission(compacting).ok === false) return { value, sent: false };
    return { value: '', sent: true };
  };
  const planIdle = planSttCommit('A', 'A', false);
  const draft = 'transcript text';
  const idleOutcome = sendWithGate(draft, planIdle, true);
  check('idle owner plans the gated prompt kind', planIdle === 'prompt');
  check('compact rejection keeps the transcript in the composer', idleOutcome.sent === false && idleOutcome.value === draft);
  const idleNotCompacting = sendWithGate(draft, planIdle, false);
  check('non-compacting prompt dispatch sends and clears the draft', idleNotCompacting.sent === true && idleNotCompacting.value === '');
}

{
  check('unconfigured live blocks before getUserMedia', planSttStart({ owner: 'A', live: { sttConfigured: false }, hasGetUserMedia: true }).ok === false && (planSttStart({ owner: 'A', live: { sttConfigured: false }, hasGetUserMedia: true }) as { message: string }).message === STT_UNCONFIGURED_MESSAGE);
  check('missing sttConfigured on a live block blocks', planSttStart({ owner: 'A', live: {}, hasGetUserMedia: true }).ok === false);
  check('absent live block with no getUserMedia is the HTTPS error', planSttStart({ owner: 'A', live: null, hasGetUserMedia: false }).ok === false && (planSttStart({ owner: 'A', live: null, hasGetUserMedia: false }) as { message: string }).message === STT_HTTPS_MESSAGE);
  check('null owner is rejected before capture', planSttStart({ owner: null, live: null, hasGetUserMedia: true }).ok === false && (planSttStart({ owner: null, live: null, hasGetUserMedia: true }) as { message: string }).message === STT_NO_SESSION_MESSAGE);
  const allowed = planSttStart({ owner: 'A', live: { sttConfigured: true }, hasGetUserMedia: true });
  check('configured live with owner starts', allowed.ok === true && allowed.ok && allowed.owner === 'A');
  check('absent live block still allows a bound owner', planSttStart({ owner: 'A', live: null, hasGetUserMedia: true }).ok === true);
  check('MIME picker takes the first supported candidate', selectSttRecorderMime((mime) => mime === 'audio/webm') === 'audio/webm');
  check('MIME picker yields undefined when nothing is supported', selectSttRecorderMime(() => false) === undefined);
  check('MIME picker yields undefined without an isTypeSupported hook', selectSttRecorderMime(undefined) === undefined);
}

{
  const convertCurrent = planSttTranscribeResult({ owner: 'A', current: 'A', wavOk: false, audioBase64: null, text: '' });
  check('WAV convert failure on the current owner errors', convertCurrent.kind === 'error' && convertCurrent.kind === 'error' && convertCurrent.message === STT_WAV_CONVERT_MESSAGE);
  check('WAV convert failure after A→B is dropped', planSttTranscribeResult({ owner: 'A', current: 'B', wavOk: false, audioBase64: null, text: '' }).kind === 'drop');
  const capCurrent = planSttTranscribeResult({ owner: 'A', current: 'A', wavOk: true, audioBase64: null, text: 'x' });
  check('over-cap WAV on the current owner errors', capCurrent.kind === 'error' && capCurrent.kind === 'error' && capCurrent.message === sttWavCapMessage());
  check('over-cap WAV after A→B is dropped', planSttTranscribeResult({ owner: 'A', current: 'B', wavOk: true, audioBase64: null, text: 'x' }).kind === 'drop');
  check('empty transcript on the current owner errors', planSttTranscribeResult({ owner: 'A', current: 'A', wavOk: true, audioBase64: 'Zg==', text: '' }).kind === 'error' && (planSttTranscribeResult({ owner: 'A', current: 'A', wavOk: true, audioBase64: 'Zg==', text: '' }) as { message: string }).message === STT_EMPTY_TRANSCRIPT_MESSAGE);
  check('empty transcript after A→B is dropped', planSttTranscribeResult({ owner: 'A', current: 'B', wavOk: true, audioBase64: 'Zg==', text: '' }).kind === 'drop');
  const committed = planSttTranscribeResult({ owner: 'A', current: 'A', wavOk: true, audioBase64: 'Zg==', text: 'hello' });
  check('current-owner transcript is delivered', committed.kind === 'transcript' && committed.kind === 'transcript' && committed.text === 'hello');
  check('switched-away transcript is dropped', planSttTranscribeResult({ owner: 'A', current: 'B', wavOk: true, audioBase64: 'Zg==', text: 'hello' }).kind === 'drop');
  check('RPC object with text is parsed', sttTranscriptText({ text: 'hello' }) === 'hello');
  check('RPC object without text is empty', sttTranscriptText({ ok: true }) === '');
  check('non-object RPC payload is empty', sttTranscriptText(null) === '' && sttTranscriptText('hello') === '');
}

{
  const mainDest = captureSttRecordingDestination('A', 'MainComposer');
  const sideDest = captureSttRecordingDestination('A', { kind: 'SideChatTab', tabId: 'alpha' });
  check('main destination captures session + MainComposer', mainDest !== null && mainDest.kind === 'MainComposer' && mainDest.kind === 'MainComposer' && mainDest.sessionId === 'A');
  check('side destination captures session + tab id', sideDest !== null && sideDest.kind === 'SideChatTab' && sideDest.kind === 'SideChatTab' && sideDest.tabId === 'alpha');
  check('empty session never captures a destination', captureSttRecordingDestination('', 'MainComposer') === null);
  check('empty side-chat tab never captures a destination', captureSttRecordingDestination('A', { kind: 'SideChatTab', tabId: '' }) === null);

  const mainWhileSideOpen = planSttTranscriptRoute({
    destination: mainDest,
    owner: 'A',
    currentSessionId: 'A',
    sideChatPanelOpen: true,
    sideChatTabs: ['alpha'],
    text: 'hello',
    streaming: false,
  });
  check(
    'main-footer recording while Side chat is open stays on main',
    mainWhileSideOpen.kind === 'mainSubmit' && mainWhileSideOpen.kind === 'mainSubmit' && mainWhileSideOpen.plan === 'prompt',
  );
  check(
    'main-footer recording while Side chat is open does not insert into Side chat',
    mainWhileSideOpen.kind !== 'sideChatInsert',
  );

  const sideThenPanelClosed = planSttTranscriptRoute({
    destination: sideDest,
    owner: 'A',
    currentSessionId: 'A',
    sideChatPanelOpen: false,
    sideChatTabs: ['alpha'],
    text: 'hello',
    streaming: false,
  });
  check(
    'side-chat recording then panel close never submits on main',
    sideThenPanelClosed.kind === 'discardClosed',
  );
  check(
    'side-chat recording then panel close is not a main submit',
    sideThenPanelClosed.kind !== 'mainSubmit',
  );

  const sideThenTabSwitch = planSttTranscriptRoute({
    destination: sideDest,
    owner: 'A',
    currentSessionId: 'A',
    sideChatPanelOpen: true,
    sideChatTabs: ['alpha', 'beta'],
    text: 'hello',
    streaming: false,
  });
  check(
    'side-tab switch during transcription stays on the starting tab',
    sideThenTabSwitch.kind === 'sideChatInsert' && sideThenTabSwitch.kind === 'sideChatInsert' && sideThenTabSwitch.tabId === 'alpha',
  );

  const sideThenTabClosed = planSttTranscriptRoute({
    destination: sideDest,
    owner: 'A',
    currentSessionId: 'A',
    sideChatPanelOpen: true,
    sideChatTabs: ['beta'],
    text: 'hello',
    streaming: false,
  });
  check(
    'closed side-chat tab discards without main submit',
    sideThenTabClosed.kind === 'discardClosed',
  );
}

if (failures.length > 0) {
  console.error('stt.test.ts FAILED:\n' + failures.join('\n'));
  process.exit(1);
}
console.log(`stt.test.ts PASSED (${ran} checks)`);
