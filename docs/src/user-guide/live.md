# Live voice

`rpi` has two voice modes that share `settings.live` but run in different
surfaces:

- **TUI hold-to-talk (`/live`)** — `live.mode = "stt"` (default). Press to
  record, release to transcribe; the final whole-utterance transcript lands in
  the composer draft for review before you press Enter.
- **Web realtime voice** — `live.mode = "realtime"`. Runs in the browser from
  the `/web` page served by `rpi --listen`, using WebRTC to a configured
  CLIProxyAPI endpoint.

`live.mode` selects which surface is active. The TUI explicitly refuses
`mode = "realtime"` and points you to the Web listener; the `/web` page only
shows the realtime mic button when `mode` is `realtime`.

## Configuration

Both modes are configured under the `live` key in `settings.json`
(`LiveSettings` / `LiveRuntimeSettings` in `crates/pi-coding/src/settings.rs`):

```json
{
  "live": {
    "enabled": true,
    "mode": "stt",
    "sttBaseUrl": "https://localhost:9000",
    "sttApiKey": "<secret>",
    "sttModel": "whisper-1",
    "realtimeBaseUrl": "",
    "realtimeApiKey": "",
    "realtimeModel": "gpt-realtime-1.5",
    "voice": "sol",
    "language": null,
    "allowInsecure": false
  }
}
```

| Key | Default | Used by | Meaning |
|-----|---------|---------|---------|
| `enabled` | `false` | both | Master switch; voice fails with an actionable message when off. |
| `mode` | `"stt"` | both | `"stt"` for TUI hold-to-talk; `"realtime"` for Web listener realtime. |
| `sttBaseUrl` | (empty) | STT | Base URL of an OpenAI-compatible speech-to-text service (`POST {base}/v1/audio/transcriptions`). |
| `sttApiKey` | (empty) | STT | Bearer key for the STT endpoint (secret; never logged, never writable through `/settings`). |
| `sttModel` | `"whisper-1"` | STT | Model name sent in the transcription request. |
| `realtimeBaseUrl` | (empty) | realtime | Base URL of the CLIProxyAPI realtime endpoint. |
| `realtimeApiKey` | (empty) | realtime | Access key for the realtime endpoint (secret). |
| `realtimeModel` | `"gpt-realtime-1.5"` | realtime | Model label sent in the realtime session payload. |
| `voice` | `"sol"` | realtime | Voice for the realtime session (`audio.output.voice` in the v1 protocol). |
| `language` | (none) | STT | Optional BCP-47 language hint. |
| `allowInsecure` | `false` | both | Permit `http://` endpoints; `https://` is required otherwise. |

### TUI STT example

```json
{
  "live": {
    "enabled": true,
    "mode": "stt",
    "sttBaseUrl": "https://localhost:9000",
    "sttApiKey": "<secret>",
    "sttModel": "whisper-1",
    "allowInsecure": false
  }
}
```

Start the TUI, then type `/live` and hold the key to talk.

### Web realtime example

```json
{
  "live": {
    "enabled": true,
    "mode": "realtime",
    "realtimeBaseUrl": "https://localhost:8317",
    "realtimeApiKey": "<secret>",
    "realtimeModel": "gpt-realtime-1.5",
    "voice": "sol",
    "allowInsecure": false
  }
}
```

Start the listener:

```console
$ rpi --listen 127.0.0.1:8765
Control plane listening on https://127.0.0.1:8765 (loopback only)
```

Open `https://127.0.0.1:8765/web` in a browser. The listener uses a
self-signed certificate by default; accept the browser warning for local
testing or provide real certificates with `--listen-cert` and `--listen-key`.
The realtime mic button appears only when `live.mode` is `realtime`.

## Validation

`validate_live_settings` checks the effective `LiveRuntimeSettings`:

- `ws://`/`wss://` URLs are always rejected — STT speaks HTTP multipart, not
  WebSocket.
- Plaintext `http://` is refused unless `allowInsecure` is explicitly true.
- For realtime, the proxy URL must be `https://` (or `http://` with
  `allowInsecure`); unsupported schemes fail with an actionable error.
- `mode = "realtime"` is rejected in the TUI; the error tells you to use the
  Web listener.
- Errors name the exact `Settings.live.*` key to fix.

## TUI hold-to-talk usage

In the TUI, `/live` opens a hold-to-talk session:

- **Press** (hold key) starts recording; a new press while recording or
  transcribing **supersedes** the previous session (its buffer is discarded
  so the old transcript cannot land in a new target).
- **Release** stops capture and transcribes the utterance.
- A **10-second no-speech watchdog** stops capture with guidance; an
  utterance shorter than 250 ms is discarded as accidental; `Esc`/teardown
  aborts without transcribing.

Capture is mono 16 kHz i16 PCM (`SAMPLE_RATE = 16_000`), held in a bounded
backlog (30 s of audio), and the STT request runs with a 30-second client
timeout. The final transcript is delivered as a `Transcript` event into the
composer draft — the user reviews and presses Enter.

## Microphone backend

Microphone capture uses `cpal` behind the `live-capture` feature. A build
without it fails with an actionable error:

```text
Microphone capture is not compiled into this build. Rebuild with
--features pi-coding/live-capture (requires cpal and, on Linux,
libasound2-dev), or use a build that ships it
```

(`open_capture` in `crates/pi-coding/src/live.rs`).

## Web realtime flow

When `live.mode = "realtime"` and the `/web` page is open:

1. The browser creates an `RTCDataChannel('oai-events')` on the
   `RTCPeerConnection` **before** calling `createOffer`, so the data channel is
   negotiated in the SDP.
2. The mic button sends `realtime_create_call` over the WebSocket control
   plane with the browser's SDP offer.
3. The backend POSTs `{ sdp: <offer>, session: { type: "quicksilver", model,
   audio: { input: { format: { type: "audio/pcm", rate: 24000 } }, output:
   { voice } } } }` to
   `{live.realtimeBaseUrl}/v1/realtime/calls`, using `live.realtimeApiKey` on
   the server side.
4. The backend returns the SDP answer to the browser; the browser sets the
   remote description. The `oai-events` data channel opens over the same peer
   connection.
5. `session.update` and all server events (input transcripts,
   `delegation.created`, errors) flow over the `oai-events` data channel;
   incoming and outgoing audio flow over the WebRTC tracks.
6. `realtime_stop` tears the peer connection and data channel down.

The realtime API key never leaves the backend; the browser only sends the SDP
offer and never opens a direct connection to `live.realtimeBaseUrl`.

## Delegation bridge

Hyper's Codex Live receives server-side `delegation.created` events; the
TUI's generic STT endpoint never emits them, so the STT bridge detects
delegation intent client-side from the transcript: a coding verb
(implement/fix/add/test/refactor/write/create/update/change/remove/delete/
debug/repair/migrate…) paired with a code-domain signal (file path,
extension, or code word) (`is_delegation_candidate` in
`crates/pi-coding/src/live.rs`). The signal sets are exact:
`DELEGATION_VERBS` and `DELEGATION_CODE_WORDS` / `DELEGATION_EXTENSIONS`
(`crates/pi-coding/src/live.rs`). The verb must appear as a whole word and the
code signal must not be the verb itself. The TUI offers the draft with a
`⟦delegate⟧` hint and submits it through the standard prompt path — the
delegation *is* an ordinary agent turn: no separate task queue, and the reply
merges into the transcript like any other turn.

In Web realtime mode, the backend emits real `delegation.created` events over
the `oai-events` data channel and the page surfaces them in the realtime
overlay.

## Invariants

- Transcripts are never auto-submitted: every utterance lands in the composer
  for review.
- STT/realtime credentials never leave the machine unencrypted: plaintext
  endpoints are refused unless `allowInsecure` is explicit, and server-echoed
  secrets are scrubbed from errors.
- Capture is bounded: audio backlog, no-speech watchdog, utterance floor,
  and request timeout all have hard limits.
- A press supersedes the previous session and discards its buffer, so a stale
  transcript can never land in the new target.

## Related documentation

- [`settings-trust.md`](../reference/settings-trust.md) — `settings.json`
  and the `live` key
- [`cli-modes.md`](cli-modes.md) — `/live` in the slash-command surface
- [`web.md`](web.md) — the Web listener and `/web` client
