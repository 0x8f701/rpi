# Live voice: TUI `/live` STT and `/web` realtime

Live voice has two distinct modes. **Speech-to-text (STT)** records a terminal push-to-talk utterance and puts its transcription in the composer. **Realtime** uses browser WebRTC audio and a data channel. The terminal does not implement WebRTC.

## Terminal speech-to-text

Enable `live.enabled`, select `live.mode: "stt"`, and configure `live.sttBaseUrl` and `live.sttApiKey`. Secret fields are not writable through the ordinary `/settings` editor. Configure them privately in the active settings file.

With `/live` enabled, hold Ctrl+Space to record and release to transcribe. A new press supersedes the previous capture. Stop discards the capture. The final whole-utterance transcript enters the composer draft; this mode does not stream interim transcription into the transcript.

The endpoint receives an OpenAI-compatible multipart request to `{base}/v1/audio/transcriptions`, with `file`, `model`, and optional `language`, authenticated by a bearer header. The endpoint is user-configured; rpi does not implicitly send this audio to OpenAI. The default model label is `whisper-1`.

Limits are 30 seconds of retained mono 16 kHz audio, a 10-second no-speech watchdog, and a 30-second transcription request timeout. Utterances shorter than 250 milliseconds are not transcribed. Transcription responses and composer text are bounded.

HTTPS is required unless `live.allowInsecure` explicitly permits the insecure endpoint. Do not use a remote plaintext endpoint for sensitive audio or credentials.

## Browser realtime

Set `live.enabled` and select `live.mode: "realtime"`, then open the Web listener in a browser with microphone access. Realtime availability is based on advertised runtime settings; absent or unknown settings do not silently enable it.

The WebRTC session carries instructions, output voice, and client delegation. Model selection belongs to the configured server/proxy. Input transcription distinguishes deltas, replacement segments, final text, and errors. Assistant output transcription is not inserted as the user's composer draft.

Create, stop, and cleanup commands stay bound to the session captured when the call began. Switching the displayed session must not attach an existing call to the new session.

## Safety and troubleshooting

If terminal configuration selects realtime, rpi directs you to the Web listener instead of pretending the terminal can run it. If capture reports no speech, release and hold again. Check microphone permission, configured mode, endpoint reachability, and credentials separately.

Audio goes to the configured service. Review its privacy policy and protect endpoint credentials. Use [Web pairing](web.md#connection-and-pairing) to secure control of the listener.
