# Live voice (`/live`)

`/live` is a minimal hold-to-talk realtime voice mode: microphone capture →
speech-to-text → composer draft (`crates/pi-coding/src/live.rs`). It is
modeled on Hyper's Codex Live but deliberately simple: press to record,
release to transcribe, and the final whole-utterance transcript lands in the
composer draft for the user to review before pressing Enter. It is **never
auto-submitted**.

## Configuration

`/live` requires explicit configuration in `settings.json`
(`LiveRuntimeSettings` in `crates/pi-coding/src/settings.rs:425-429`):

```json
{
  "live": {
    "enabled": true,
    "sttBaseUrl": "https://your-whisper-host",
    "sttApiKey": "<secret>",
    "sttModel": "whisper-1",
    "language": null,
    "allowInsecure": false
  }
}
```

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | `false` | Master switch; `/live` fails with an actionable message when off. |
| `sttBaseUrl` | (empty) | Base URL of an OpenAI-compatible speech-to-text service. |
| `sttApiKey` | (empty) | Bearer key for that service (secret; never logged, never writable through `/settings`). |
| `sttModel` | `whisper-1` | Model name sent in the transcription request. |
| `language` | (none) | Optional language hint. |
| `allowInsecure` | `false` | Permit `http://` endpoints (loopback/self-hosted); `https://` is required otherwise. |

Validation (`validate_live_settings` in `live.rs:97-129`):

- `ws://`/`wss://` URLs are always rejected — `/live` speaks HTTP multipart to
  `{base}/v1/audio/transcriptions`, not WebSocket.
- Plaintext `http://` is refused unless `allowInsecure` is explicitly true.
- Errors name the exact `Settings.live.*` key to fix.

## Usage

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

(`open_capture` in `live.rs:189-201`).

## Delegation bridge

Hyper's Codex Live receives server-side `delegation.created` events; rpi's
generic STT endpoint never emits them, so the bridge detects delegation
intent client-side from the transcript: a coding verb (implement/fix/add/
test/refactor/write/create/update/change/remove/delete/debug/repair/migrate…)
paired with a code-domain signal (file path, extension, or code word)
(`is_delegation_candidate` in `live.rs:681`). The signal sets are exact
(`live.rs:681-699`): `DELEGATION_VERBS` = `implement, fix, add, test,
refactor, write, create, update, change, remove, delete, debug, repair,
migrate, optimize, rewrite, extend, port`; `DELEGATION_CODE_WORDS` covers
code-domain nouns (`function`, `class`, `struct`, `module`, `api`,
`endpoint`, `cli`, `bug`, `test`, `config`, …); `DELEGATION_EXTENSIONS`
covers `.rs`, `.py`, `.ts`, `.js`, `.go`, `.c`, `.md`, `.sh`, `.json`,
`.toml`, … — a bare `config`/`path` word or a slash-containing token also
counts as a code signal. The verb must appear as a whole word (boundary
matched) and the code signal must not be the verb itself
(`has_code_signal`, `live.rs:730-739`). The TUI offers the draft with a
`⟦delegate⟧` hint and submits it through the standard prompt path — the
delegation *is* an ordinary agent turn: no separate task queue, and the reply
merges into the transcript like any other turn.

## Invariants

- Transcripts are never auto-submitted: every utterance lands in the
  composer for review.
- STT credentials never leave the machine unencrypted: plaintext endpoints
  are refused unless `allowInsecure` is explicit, and server-echoed secrets
  are scrubbed from errors.
- Capture is bounded: audio backlog, no-speech watchdog, utterance floor,
  and request timeout all have hard limits.
- A press supersedes the previous session and discards its buffer, so a
  stale transcript can never land in the new target.

## Related documentation

- [`settings-trust.md`](../reference/settings-trust.md) — `settings.json`
  and the `live` key
- [`cli-modes.md`](cli-modes.md) — `/live` in the slash-command surface
