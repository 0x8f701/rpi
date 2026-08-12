#!/usr/bin/env python3
"""Shared loopback OpenAI-compatible mock provider for user-perspective tmux E2E.

Serves deterministic SSE responses on localhost and writes the chosen port to
`--port-file` so a bash scenario can build `models.json` and launch rpi against
it. Scenarios:

- steering:  1st request streams slowly (keeps is_streaming true so a typed
              follow-up lands in the queue), later requests reply instantly.
- bash-card: 1st request returns a `bash` tool call with a multi-line command
              carrying leading comment lines; 2nd returns assistant text whose
              code fence never closes; later requests reply instantly.
- todo-dag:  1st request returns a `todo` init tool call (phases + tasks);
              later requests reply instantly.
- workflow:  supervisor planning requests (prompt contains "You supervise
              workflow") get a `todo` init call plus a `bash` call that commits
              a plan file inside the workflow worktree; follow-up supervisor
              requests (with todo results) get plain text. Worker requests
              (prompt carries <delegated_assignment>) get completion text.
              With `--hold-workers`, worker completions are held open until the
              campaign POSTs /__release (deterministic non-terminal DAG seam
              used by the workflow RPC campaign). ThreadingHTTPServer serves
              every scenario because held streams need concurrent control.
              Any other request gets plain text.
- sessions:  replies route by exact current prompt. `sessions-slow-b3` emits
              its first delta, then waits for POST /__release-session so the
              Web lane owns the busy-close/release order.
- xss:       every request streams assistant text containing raw HTML that
              must never execute (<img onerror>, <script> setting a global)
              plus a credential-shaped token the web client must redact to
              [REDACTED] in every view.
- scroll:    replies route by exact current prompt. `scroll-long-a` streams
              a long slow stream (44 indexed chunks, 0.35s cadence) so the
              Web scroll-pinning lane has a tall, long-lived transcript;
              `scroll-echo` replies instantly for the second session.
- slowclient: replies route by exact current prompt (slow-client web lane):
              `slowclient-burst-<N>-<bytes>` streams N text deltas back-to-
              back (one HTTP body, no sleeps — an event burst; tail marker
              "slowclient-done-<N>", chunk prefix "s<N>:" — unique per
              burst); `slowclient-burstslow-<N>-<bytes>-<delay>` writes the
              same N deltas at a <delay>s cadence so a browser-side
              main-thread stall overlaps a sustained flood;
              `slowclient-heavy-<count>` returns ONE final message with
              <count> KaTeX blocks, a <count>-node mermaid diagram and a
              table (heavy synchronous finalize + async mermaid hydration).
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import struct
import sys
import threading
import time
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

SUPERVISOR_RE = re.compile(r"You (?:supervise|plan) workflow\s+([^\s.]+)\.\s*Objective:\s*([^\n<]+)")
DELEGATED_RE = re.compile(r"<delegated_assignment>\s*(.*?)\s*</delegated_assignment>", re.DOTALL)

# Deterministic control seams: a selected completion stream sends SSE headers
# and its first delta, then waits for its scenario-specific release POST.
# Workflow and Web sessions deliberately use separate events/endpoints so one
# protocol cannot release the other's held streams.
HOLD_WORKERS = False
HELD_STREAMS: list[Any] = []
WORKER_RELEASE = threading.Event()
SESSION_RELEASE = threading.Event()
WORKER_RELEASE_PATH = "/__release"
SESSION_RELEASE_PATH = "/__release-session"


class HeldStream:
    def __init__(
        self,
        wfile: Any,
        request_number: int,
        first_text: str,
        final_text: str,
        release: threading.Event,
    ) -> None:
        self.wfile = wfile
        self.request_number = request_number
        self.first_text = first_text
        self.final_text = final_text
        self.release = release

BASH_CARD_COMMAND = (
    "# QA shell probe\n"
    "# second probe line\n"
    "printf 'card-one\\n'; printf 'card-two\\n'"
)

WORKFLOW_COMMIT_COMMAND = (
    "printf 'plan\\n' > PLAN.e2e && "
    "git add PLAN.e2e && "
    "git commit -m 'e2e plan' -q && "
    "git rev-parse --short HEAD"
)

UNCLOSED_FENCE_TEXT = (
    "Here is the snippet:\n"
    "```rust\n"
    "fn main() {\n"
    "    let x = 1;\n"
    "}\n"
)

# Web XSS lane fixture: raw HTML that must render as inert text (never as a
# live element/script) and a credential-shaped string the client must redact.
# Single line on purpose: the web client renders it inside one assistant
# message; the assertions look for the escaped literal and for [REDACTED].
# The credential prefix is assembled from single chars so the SOURCE has no
# contiguous token-shaped literal for a credential scan to match; the runtime
# value still matches the web redaction pattern (-> [REDACTED]).
_SK = "s" + "k" + "-"
XSS_PAYLOAD_TEXT = (
    "unsafe <img src=x onerror=alert(1)><script>window.__xss='pwned'</script> "
    "and the leaked credential " + _SK + "test-secret-abcdef0123456789."
)

# Web transcript-renderer fixture: markdown table + task list, a mermaid
# flowchart fence, inline $...$ math and a $$...$$ display block, plus an
# ordinary text fence and a Rust fence whose metadata exercises the registered
# language path (`rust,ignore`). The Rust body includes tokens near its end so
# the browser lane proves the whole block—not only an initial fragment—was
# highlighted. Links/images cover the URL policy. Raw string so backslashes in
# the LaTeX survive; returned only when the prompt asks for "render rich
# content" (never by request number, so other steering lanes keep their
# odd/even slow/instant cadence untouched).
# The fences/links/images render inert (no raw markers in textContent) and only
# ADD observable elements, so the existing assertions still hold.
RICH_TEXT = r"""## Rendering upgrade e2e

| Name  | Value |
|-------|-------|
| speed | c     |
| mass  | m     |

- [x] completed task
- [ ] open task

```mermaid
flowchart LR
  A[Start] --> B{Check}
  B -->|ok| C[End]
  B -->|no| D[Retry]
```

Inline math $E=mc^2$ renders, and a display block:

$$
\int_0^\infty e^{-x^2}\,dx = \frac{\sqrt{\pi}}{2}
$$

Renderer coverage — ordinary code fence, link/image URL policy:

```text
const answer = 42
```

```rust,ignore
#[derive(Debug)]
struct Packet<'a> { label: &'a str }

async fn decode(packet: Option<Packet<'_>>) -> Result<&str, String> {
    match packet {
        Some(value) => Ok(value.label),
        None => Err(String::from("missing")),
    }
}
```

Safe link [pi web docs](https://example.org/pi-web) and a blocked
[evil](javascript:alert(1)) link that must not become a live anchor.

Safe image ![pic](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=)
and a blocked ![x](data:text/html,<b>hi</b>) image that must stay inert.
"""

# A second rich-content payload (web coverage prompt "render rich content
# extra"): drives markdown branches RICH_TEXT does not — an <hr>, a
# blockquote, ordered + nested lists, task-glyph variants, an
# unregistered-language fence (highlightAuto path), a lang-less fence (text
# label), an EMPTY mermaid fence (async hydration's empty-source branch) and
# an invalid-diagram mermaid fence (error host), a currency-shaped amount
# (extractMath's digit-preceding-$ skip), and extra URL-policy cases
# (relative image, http image, mailto + relative links).
RICH_TEXT_EXTRA = r"""## Rendering upgrade e2e — extra branches

---

> Blockquote line one
> Blockquote line two

1. ordered first
2. ordered second
   - nested bullet a
   - nested bullet b
- [X] checked task
- [ ] unchecked task

```weirdlang
not a registered language — highlightAuto path
```

```
lang-less fence — text label
```

```mermaid

```

```mermaid
sequenceDiagram
  Alice->John: hi
  bad token here
```

Price 5$/unit (currency, not math) and inline $x^2$ (math).

![rel](./images/pic.png) ![http](https://example.org/pic.png) ![blocked](javascript:alert(1))

[mail](mailto:web-e2e@example.org) [rel](../docs/guide.md)
"""


# Web transcript scroll-pinning lane (E2E.d/web/scroll.sh -> scroll_test.mjs).
# A LONG slow stream: 44 indexed chunks (~240 chars each, 0.35s cadence,
# ~15.4s in-flight) so the transcript overflows the viewport early and stays
# growing through the whole pinned/unpinned assertion window. The tail chunk
# carries the completion marker; every other chunk carries an ascending
# index the lane waits on. Routed by EXACT prompt text ("scroll-long-a").
SCROLL_LONG_CHUNKS = 44
SCROLL_LONG_DELAY = 0.35
SCROLL_LONG_FILLER = (
    "The quick brown fox jumps over the lazy dog while the streaming "
    "transcript stays pinned to the bottom of the viewport without ever "
    "jumping mid-stream; this chunk keeps the transcript growing for the "
    "scroll-pinning lane. "
)

# Deterministic solid-color PNG (640x360) as a data URI. The final-markdown
# message embeds it: the <img> decodes ASYNC after the React commit and grows
# the content height with no item change — the ResizeObserver re-pin is the
# only thing that can keep the view glued (the old nearBottom/useEffect logic
# has no observer and drifts).
def _solid_png_data_uri(width: int, height: int, rgb: tuple[int, int, int]) -> str:
    def chunk(kind: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        )

    raw = b"".join(b"\x00" + bytes(rgb) * width for _ in range(height))
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    return "data:image/png;base64," + base64.b64encode(png).decode("ascii")


SCROLL_FINAL_MD_IMAGE = _solid_png_data_uri(640, 360, (16, 42, 64))

# "scroll-final-md": streams raw markdown text in deltas, then message_end
# commits a FINAL render that is much taller than the streamed text — a tall
# rust code fence (synchronous commit growth), a Mermaid fence (async SVG
# hydration after the commit), and a data-URL image (async decode). Each is a
# distinct height-mutation class the lane classifies: react-commit,
# mermaid-hydration, image-decode. The tail paragraph carries the completion
# marker. Routed by EXACT prompt text ("scroll-final-md").
SCROLL_MD_DELAY = 0.35
SCROLL_FINAL_MD = f"""## Streaming final render

This assistant message streams raw text first, then the final render commits
a tall code fence, a Mermaid diagram that hydrates asynchronously, and a
data-URL image that decodes asynchronously. The pinned view must stay glued
through every one of those height mutations.

```rust,ignore
fn scroll_pinned(packets: &[Packet]) -> Result<&str, String> {{
    let label = packets.first().map(|p| p.label.as_str()).unwrap_or("none");
    if label.is_empty() {{
        return Err(String::from("missing"));
    }}
    Ok(label)
}}

#[derive(Debug)]
struct Packet {{
    label: String,
    seq: u64,
    delta: u64,
}}
```

```mermaid
graph TD
  A[stream start] --> B{{pinned?}}
  B -- yes --> C[glue to bottom]
  B -- no --> D[freeze viewport]
  C --> E[async mermaid height]
  D --> E
```

![async decode]({SCROLL_FINAL_MD_IMAGE})

Tail marker: scroll-final-md-done"""

# Paragraph-boundary split: each part streams as one text_delta; the final
# part carries finish_reason "stop" and the full text is the concatenation.
SCROLL_FINAL_MD_PARTS = [part.strip() + "\n\n" for part in SCROLL_FINAL_MD.split("\n\n") if part.strip()]

# "scroll-narrow": a short stream (8 chunks, 0.3s) for the narrow/mobile
# viewport phase — header badge + Abort button toggle at turn_start/turn_end
# and the composer reflow are measured there while the pin must stay bounded
# (pinned) or exactly frozen (unpinned).
SCROLL_NARROW_CHUNKS = 8
SCROLL_NARROW_DELAY = 0.3
SCROLL_NARROW_FILLER = SCROLL_LONG_FILLER  # content-routed, exact prompt only


def stream_response(payloads: list[dict[str, Any]]) -> bytes:
    rows = [f"data: {json.dumps(payload, separators=(',', ':'))}\n\n" for payload in payloads]
    rows.append("data: [DONE]\n\n")
    return "".join(rows).encode()


def stream_held_response(wfile: Any, held: HeldStream) -> None:
    """Serve one control-gated SSE stream.

    HTTP/1.0 close-delimits the body (no content-length). The initial delta
    proves the provider entered the turn; no terminal delta can be emitted
    until the owning release event fires. Periodic comments keep the pipe
    exercised so an abort or process exit surfaces as a broken pipe.
    """
    first = text_payload(f"user-mock-held-{held.request_number}", held.first_text)
    first["choices"][0]["finish_reason"] = None
    try:
        wfile.write(f"data: {json.dumps(first, separators=(',', ':'))}\n\n".encode())
        wfile.flush()
        while not held.release.wait(0.25):
            wfile.write(b": keepalive\n\n")
            wfile.flush()
        final = text_payload(f"user-mock-held-{held.request_number}", held.final_text)
        wfile.write(f"data: {json.dumps(final, separators=(',', ':'))}\n\n".encode())
        wfile.write(b"data: [DONE]\n\n")
        wfile.flush()
    except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError, OSError):
        # The client aborted the turn (pause/cancel/exit); nothing to deliver.
        return


def burst_stream_response(wfile: Any, payloads: list[dict[str, Any]]) -> None:
    """Write an SSE burst as fast as the socket allows. The whole body is
    pre-built ONCE (no per-chunk json.dumps/write/flush Python overhead — the
    slow-client web lane floods tens of thousands of deltas), then written in
    ~512KB slices; TCP backpressure paces the write when the receiver stalls.
    HTTP/1.0 close-delimits the body."""
    rows = [f"data: {json.dumps(payload, separators=(',', ':'))}\n\n" for payload in payloads]
    rows.append("data: [DONE]\n\n")
    body = "".join(rows).encode()
    slice_size = 524288
    for offset in range(0, len(body), slice_size):
        wfile.write(body[offset:offset + slice_size])
        wfile.flush()


def slow_stream_response(wfile: Any, payloads: list[dict[str, Any]], delay: float) -> None:
    """Write an SSE stream incrementally to a handler's wfile, sleeping between
    chunks so the client observes a genuinely in-flight turn (the steering
    queue window depends on the stream not finishing instantly)."""
    for index, payload in enumerate(payloads):
        if index > 0:
            time.sleep(delay)
        chunk = f"data: {json.dumps(payload, separators=(',', ':'))}\n\n".encode()
        wfile.write(chunk)
        wfile.flush()
    wfile.write(b"data: [DONE]\n\n")
    wfile.flush()


def tool_call_payload(rid: str, call_id: str, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": rid,
        "model": "user-mock",
        "choices": [
            {
                "index": 0,
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": json.dumps(arguments, separators=(",", ":")),
                            },
                        }
                    ]
                },
                "finish_reason": "tool_calls",
            }
        ],
        "usage": {"prompt_tokens": 8, "completion_tokens": 4, "total_tokens": 12},
    }


def text_payload(rid: str, text: str) -> dict[str, Any]:
    return {
        "id": rid,
        "model": "user-mock",
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 8, "completion_tokens": len(text), "total_tokens": 8 + len(text)},
    }


def todo_init(phases: list[tuple[str, list[str]]]) -> dict[str, Any]:
    return {
        "op": "init",
        "list": [{"phase": phase, "items": items} for phase, items in phases],
        "task": None,
        "phase": None,
        "items": None,
        "dependsOn": None,
        "cascade": None,
    }


def last_user_text(body: dict[str, Any]) -> str:
    """Text of the last REAL user input. Provider bodies inject goal/loop
    reminders as user-role messages (`<system-reminder>…</system-reminder>`);
    those are model context, not input, so they are skipped when locating the
    user's latest prompt — routing and queue-ordering assertions need the
    actual typed text."""
    for message in reversed(body.get("messages") or []):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, str):
            if content.lstrip().startswith("<system-reminder>"):
                continue
            return content
        if isinstance(content, list):
            text = " ".join(
                str(block.get("text", ""))
                for block in content
                if isinstance(block, dict) and block.get("type") == "text"
            )
            if text.lstrip().startswith("<system-reminder>"):
                continue
            return text
    return ""


def user_text_digest(text: str) -> str:
    """Deterministic 12-hex sha256 marker for one request's last real user
    input. Queue-order assertions compare digests across requests; the
    digest never echoes prompt content into the evidence log."""
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:12]


def message_text(body: dict[str, Any]) -> str:
    fragments: list[str] = []
    for message in body.get("messages") or []:
        content = message.get("content")
        if isinstance(content, str):
            fragments.append(content)
        elif isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and isinstance(block.get("text"), str):
                    fragments.append(block["text"])
    return "\n".join(fragments)


def tool_names(body: dict[str, Any]) -> set[str]:
    return {
        str(item.get("function", {}).get("name"))
        for item in body.get("tools") or []
        if item.get("type") == "function"
    }


def completed_tool_results(body: dict[str, Any], tool_name: str) -> list[str]:
    calls: dict[str, str] = {}
    results: dict[str, str] = {}
    for message in body.get("messages") or []:
        if message.get("role") == "assistant":
            for call in message.get("tool_calls") or []:
                function = call.get("function") or {}
                if function.get("name") == tool_name and call.get("id"):
                    calls[str(call["id"])] = tool_name
        elif message.get("role") == "tool" and message.get("tool_call_id"):
            results[str(message["tool_call_id"])] = str(message.get("content") or "")
    return [results[call_id] for call_id in calls if call_id in results]

def last_real_message_is_tool_result(body: dict[str, Any]) -> bool:
    """True when the most recent real (non-system-reminder) message in the
    request body is a tool result — i.e. the agent already ran the tool and
    is now requesting the follow-up reply. This is the correct two-call
    discriminator for content-routed markers that share a tool name across
    turns (e.g. multiple bash/read/process markers in one session):
    `completed_tool_results` would match ANY prior result of that tool and
    wrongly short-circuit the second turn, so markers that can repeat use
    this instead."""
    for message in reversed(body.get("messages") or []):
        role = message.get("role")
        if role == "tool":
            return True
        if role == "assistant":
            return False
        if role == "user":
            content = message.get("content")
            if isinstance(content, str):
                if content.lstrip().startswith("<system-reminder>"):
                    continue
                return False
            if isinstance(content, list):
                text = " ".join(
                    str(block.get("text", ""))
                    for block in content
                    if isinstance(block, dict) and block.get("type") == "text"
                )
                if text.lstrip().startswith("<system-reminder>"):
                    continue
                return False
            return False
    return False


class UserMockServer(BaseHTTPRequestHandler):
    scenario: str = ""
    serial = 0

    def do_POST(self) -> None:
        release = None
        if self.path == WORKER_RELEASE_PATH and self.scenario == "workflow" and HOLD_WORKERS:
            release = WORKER_RELEASE
        elif self.path == SESSION_RELEASE_PATH and self.scenario == "sessions":
            release = SESSION_RELEASE
        if release is not None:
            release.set()
            body = b'{"released": true}\n'
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        # Codex Live realtime create-call proxy endpoint (web realtime
        # coverage driver). The Rust realtime_create_call proxy (rpc.rs)
        # POSTs a JSON body exactly {sdp, session} (Content-Type:
        # application/json) with `OpenAI-Alpha: quicksilver=v2` and a Bearer
        # token to `{realtimeBaseUrl}/v1/realtime/calls`. Reply with a
        # Location header carrying an `rtc_` call id plus a bare SDP answer
        # body; the E2E driver stubs the browser RTCPeerConnection, so the
        # answer only needs to be a non-empty SDP-looking string.
        if self.path == "/v1/realtime/calls":
            length = int(self.headers.get("content-length", "0"))
            raw_body = self.rfile.read(length)
            # The create-call contract is a JSON body exactly {sdp, session};
            # validate it before answering so a contract drift (multipart,
            # missing fields, non-object session) fails loudly as a 400 rather
            # than being silently consumed. Never echo the body or auth values.
            try:
                payload = json.loads(raw_body)
                sdp_value = payload.get("sdp") if isinstance(payload, dict) else None
                session_value = payload.get("session") if isinstance(payload, dict) else None
                if not isinstance(sdp_value, str) or not sdp_value.strip():
                    raise ValueError("sdp must be a non-empty string")
                if not isinstance(session_value, dict):
                    raise ValueError("session must be an object")
            except (ValueError, json.JSONDecodeError):
                bad = b"mock realtime create-call requires a JSON body {sdp: string, session: object}\n"
                self.send_response(400)
                self.send_header("content-type", "text/plain")
                self.send_header("content-length", str(len(bad)))
                self.end_headers()
                self.wfile.write(bad)
                return
            # Error-path knob (web realtime_rpc lane): MOCK_REALTIME_ERROR=1
            # makes the create-call endpoint reject with 500 so the Web
            # client's realtime_create_call proxy fails and the page surfaces
            # the user-visible "realtime call failed" toast. Default off:
            # the coverage/webrtc flows rely on the 200 bare-SDP answer.
            if os.environ.get("MOCK_REALTIME_ERROR") == "1":
                body = b"mock realtime create-call rejected\n"
                self.send_response(500)
                self.send_header("content-type", "text/plain")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            type(self).serial += 1
            call_id = f"rtc_e2e-{type(self).serial}"
            # Record the proxy's required request headers for the E2E
            # assertion (the web realtime coverage driver asserts
            # OpenAI-Alpha: quicksilver=v2, the Bearer token, and the JSON
            # Content-Type reached the backend). Persisted when
            # MOCK_REALTIME_EVIDENCE is set.
            alpha = self.headers.get("OpenAI-Alpha", "")
            auth = self.headers.get("Authorization", "")
            content_type = self.headers.get("Content-Type", "")
            print(
                f"user-mock scenario={type(self).scenario} realtime_create_call "
                f"call={call_id} alpha={alpha!r} auth_present={bool(auth)} "
                f"content_type={content_type!r}",
                file=sys.stderr,
                flush=True,
            )
            evidence_file = os.environ.get("MOCK_REALTIME_EVIDENCE", "")
            if evidence_file:
                try:
                    with open(evidence_file, "w", encoding="utf-8") as handle:
                        json.dump(
                            {
                                "callId": call_id,
                                "openaiAlpha": alpha,
                                "authPresent": bool(auth),
                                "contentType": content_type,
                            },
                            handle,
                        )
                except OSError as error:
                    print(
                        f"user-mock scenario={type(self).scenario} "
                        f"realtime_evidence_write_error={type(error).__name__}",
                        file=sys.stderr,
                        flush=True,
                    )
            body = b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=web-e2e-realtime-answer\r\n"
            self.send_response(200)
            self.send_header("location", f"/v1/realtime/calls/{call_id}")
            self.send_header("content-type", "application/sdp")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        # Hold-to-talk STT proxy endpoint (web stt_rpc lane). The Rust
        # stt_transcribe RPC (rpc.rs) forwards the browser's WAV as an
        # OpenAI-compatible multipart form ({file: audio/wav, model}) with a
        # Bearer token; reply with {"text": ...}. The browser never contacts
        # this endpoint directly and never holds the key, so the evidence
        # records only metadata — never the auth value or the audio body.
        if self.path == "/v1/audio/transcriptions":
            length = int(self.headers.get("content-length", "0"))
            raw = self.rfile.read(length)
            auth = self.headers.get("Authorization", "")
            content_type = self.headers.get("Content-Type", "")
            # Evidence is written BEFORE the error knob so the error phase
            # also proves the Rust proxy reached the mock with the server-held
            # key. Metadata only — never the auth value or the audio body.
            evidence_file = os.environ.get("MOCK_STT_EVIDENCE", "")
            if evidence_file:
                try:
                    with open(evidence_file, "w", encoding="utf-8") as handle:
                        json.dump(
                            {
                                "authPresent": bool(auth),
                                "contentType": content_type,
                                "filePresent": b'name="file"' in raw,
                                "wavPresent": b"RIFF" in raw,
                                "modelPresent": b'name="model"' in raw,
                            },
                            handle,
                        )
                except OSError as error:
                    print(
                        f"user-mock scenario={type(self).scenario} "
                        f"stt_evidence_write_error={type(error).__name__}",
                        file=sys.stderr,
                        flush=True,
                    )
            # Error-path knob (web stt_rpc lane): MOCK_STT_ERROR=1 makes the
            # endpoint reject with 500 so the page surfaces the bounded
            # "transcription failed" toast.
            if os.environ.get("MOCK_STT_ERROR") == "1":
                body = b'{"error": {"message": "mock stt rejected"}}\n'
                self.send_response(500)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            body = b'{"text": "web stt transcript"}\n'
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length))
        type(self).serial += 1
        request_number = type(self).serial
        # One line per request, stderr (every scenario redirects it to its
        # evidence log). Scenarios use the digest of each request's last user
        # text to prove queue ordering: a follow-up typed mid-turn must NOT
        # appear in the in-flight request's body and must be the LAST user
        # message of the next request (drained after the turn). Only the
        # bounded length and the digest are logged — never the prompt itself.
        user_text = last_user_text(body)
        print(
            f"user-mock scenario={self.scenario} request#{request_number} "
            f"user_len={len(user_text)} user_digest={user_text_digest(user_text)}",
            file=sys.stderr,
            flush=True,
        )
        try:
            response = self.route(request_number, body)
        except BaseException as error:  # never let a routing bug wedge the client
            # Exception class only: the repr could embed request bodies or
            # paths that must never reach the evidence log.
            print(
                f"user-mock scenario={self.scenario} request#{request_number} "
                f"error={type(error).__name__}",
                file=sys.stderr,
                flush=True,
            )
            response = stream_response([text_payload(f"user-mock-err-{request_number}", "mock rejected")])
        # Routing kind for the campaign's planner-engagement evidence: held
        # workflow streams are worker turns; the sessions barrier is merely a
        # held provider response. Supervisor planning retains its own kind.
        if isinstance(response, HeldStream):
            kind = "worker" if self.scenario == "workflow" else "held"
        elif "You plan workflow" in message_text(body):
            kind = "planning"
        else:
            kind = "other"
        print(
            f"user-mock scenario={self.scenario} request#{request_number} kind={kind}",
            file=sys.stderr,
            flush=True,
        )
        if response is None:
            # Slow routes wrote the complete HTTP response and streamed their
            # body directly. Never append a second status line to that SSE
            # body; the handler return closes the HTTP/1.0 response cleanly.
            return
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        if isinstance(response, HeldStream):
            # Headers and first delta precede the control wait; the terminal
            # delta is impossible until the scenario-specific release POST.
            self.end_headers()
            stream_held_response(self.wfile, response)
            return
        self.send_header("content-length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)

    def route(self, request_number: int, body: dict[str, Any]) -> bytes:
        scenario = type(self).scenario
        if scenario == "xss":
            # The full hostile payload in one assistant text block; the web
            # client must escape it (inert text) and redact the credential-shaped secret.
            return stream_response([text_payload(f"user-mock-{request_number}", XSS_PAYLOAD_TEXT)])
        if scenario == "steering":
            if "render rich content" in last_user_text(body):
                return stream_response([text_payload(f"user-mock-{request_number}", RICH_TEXT)])
            # Web coverage markdown-branch driver: the second rich payload
            # (hr/blockquote/nested-list/unregistered-fence/empty+invalid
            # mermaid/currency/URL-policy). Assertions live in coverage_test.mjs
            # (feature "markdown", IDs md.extra-*). Marker deliberately does
            # NOT contain "render rich content" (substring routing).
            if "render markdown extra branches" in last_user_text(body):
                return stream_response([text_payload(f"user-mock-{request_number}", RICH_TEXT_EXTRA)])
            # Presentation tool-card seeds (coverage command-card driver):
            # each marker emits one real tool call and routes the completed
            # tool result through completed_tool_results so the runtime
            # executes the REAL tool and the transcript renders the REAL card
            # state (two-call pattern, exact markers).
            if "presentation bash success" in last_user_text(body):
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "bash success complete")])
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-presentation-bash-{request_number}",
                            "bash",
                            {
                                "command": (
                                    "# line one\n"
                                    "# line two\n"
                                    "# line three\n"
                                    "# line four\n"
                                    "# line five\n"
                                    "printf 'command-ok\\n'"
                                )
                            },
                        )
                    ]
                )
            if "presentation bash error" in last_user_text(body):
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "bash error complete")])
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-presentation-bash-err-{request_number}",
                            "bash",
                            {"command": "exit 7"},
                        )
                    ]
                )
            if "presentation write success" in last_user_text(body):
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "write success complete")])
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-presentation-write-{request_number}",
                            "write",
                            {
                                "path": "notes.txt",
                                "content": "".join(f"write payload line {n}\n" for n in range(1, 41)),
                            },
                        )
                    ]
                )
            if "presentation read image" in last_user_text(body):
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "read image complete")])
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-presentation-read-{request_number}",
                            "read",
                            {"path": "logo.png"},
                        )
                    ]
                )
            if "presentation thinking turn" in last_user_text(body):
                # Real thinking stream: 3 reasoning_content deltas (~0.4s
                # cadence) then 2 content deltas, finish_reason "stop". The
                # presentation lane's fixture must run MOCK_REASONING=1 so
                # the runtime parses reasoning_content into a thinking block.
                # The reasoning deltas carry LITERAL backslash-n sequences
                # (the two characters `\n` after JSON parsing — a real-model
                # escaped-newline artifact) so the client's conservative
                # normalization must turn them into real multi-line body
                # text; the middle step appends a long unbroken run that a
                # 390px viewport must WRAP (overflow-wrap), never widen.
                payloads = []
                for step in [
                    "reasoning step one\\n",
                    "reasoning step two " + "x" * 120 + "\\n",
                    "推理第三段 reasoning step three",
                ]:
                    payloads.append(
                        {
                            "id": f"user-mock-{request_number}-r{len(payloads)}",
                            "model": "user-mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"reasoning_content": step},
                                    "finish_reason": None,
                                }
                            ],
                            "usage": {"prompt_tokens": 8, "completion_tokens": len(step), "total_tokens": 8 + len(step)},
                        }
                    )
                for chunk in ["final answer ", "thinking-turn-done"]:
                    payloads.append(
                        {
                            "id": f"user-mock-{request_number}-c{len(payloads)}",
                            "model": "user-mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"content": chunk},
                                    "finish_reason": None,
                                }
                            ],
                            "usage": {"prompt_tokens": 8, "completion_tokens": len(chunk), "total_tokens": 8 + len(chunk)},
                        }
                    )
                payloads[-1]["choices"][0]["finish_reason"] = "stop"
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                slow_stream_response(self.wfile, payloads, delay=0.4)
                return None
            if "presentation think bash" in last_user_text(body):
                # Real thinking + streamed tool-call turn: reasoning_content
                # deltas first, then the bash arguments arrive as INCREMENTAL
                # tool_calls fragments (one ToolCallDelta per chunk, exactly
                # like a real model streaming `{"command": …}`), so the web
                # client's toolcall_delta path is genuinely exercised. The
                # command itself sleeps, so the structured Command card stays
                # visibly running while the E2E asserts no raw JSON / command
                # args ever leaked into the transcript.
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "think bash complete")])
                payloads = []
                for step in [
                    "planning tool use one\\n",
                    "reasoning about the bash command\\n",
                    "第三段推理 think-bash thinking\\n",
                ]:
                    payloads.append(
                        {
                            "id": f"user-mock-{request_number}-r{len(payloads)}",
                            "model": "user-mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"reasoning_content": step},
                                    "finish_reason": None,
                                }
                            ],
                            "usage": {"prompt_tokens": 8, "completion_tokens": len(step), "total_tokens": 8 + len(step)},
                        }
                    )
                call_id = f"call-think-bash-{request_number}"
                # One complete `{"command":…}` JSON split across fragments;
                # the first chunk also carries the call id + tool name.
                fragments = ['{"command":', '"sleep 4 && printf \'think-bash-ok\'', '"}']
                for position, fragment in enumerate(fragments):
                    tool = {"index": 0, "function": {"arguments": fragment}}
                    if position == 0:
                        tool["id"] = call_id
                        tool["type"] = "function"
                        tool["function"]["name"] = "bash"
                    payloads.append(
                        {
                            "id": f"user-mock-{request_number}-t{len(payloads)}",
                            "model": "user-mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"tool_calls": [tool]},
                                    "finish_reason": "tool_calls" if position == len(fragments) - 1 else None,
                                }
                            ],
                            "usage": {"prompt_tokens": 8, "completion_tokens": 4, "total_tokens": 12},
                        }
                    )
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                slow_stream_response(self.wfile, payloads, delay=0.35)
                return None
            if "presentation process long" in last_user_text(body):
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "process long complete")])
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-presentation-long-{request_number}",
                            "process",
                            {
                                "op": "start",
                                "argv": ["true"],
                                "label": "a long dev server label for the process card equal width test",
                            },
                        )
                    ]
                )
            if "presentation process short" in last_user_text(body):
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "process short complete")])
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-presentation-short-{request_number}",
                            "process",
                            {"op": "start", "argv": ["true"], "label": "short"},
                        )
                    ]
                )
            if "presentation process error" in last_user_text(body):
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "process error complete")])
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-presentation-proc-err-{request_number}",
                            "process",
                            {
                                "argv": ["python3", "-m", "http.server", "8765", "--bind", "127.0.0.1"],
                                "label": "dev server",
                            },
                        )
                    ]
                )
            # Hub tool cards via the REAL hub tool: the loopback session is the
            # orchestration main agent, so a message-wait executes for real.
            # `presentation hub wait` waits 1.5s with no delivery → the timeout
            # card (message: null); the presentation lane asserts the humanized
            # running card in that window. `presentation hub wait typed` waits
            # 20s and the driver RPC-delivers a Main→Main mailbox message mid-
            # wait (drained by wait_message) → the settled card carries the
            # typed details.message projection. `presentation hub send`
            # addresses an unknown recipient UUID → the real tool returns a
            # failed receipt the card renders as its outcome (no raw JSON).
            if "presentation hub wait typed" in last_user_text(body):
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "hub wait typed complete")])
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-presentation-hub-wait-typed-{request_number}",
                            "hub",
                            {"op": "wait", "timeoutMs": 20000},
                        )
                    ]
                )
            if "presentation hub wait" in last_user_text(body):
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "hub wait complete")])
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-presentation-hub-wait-{request_number}",
                            "hub",
                            {"op": "wait", "timeoutMs": 1500},
                        )
                    ]
                )
            if "presentation hub send" in last_user_text(body):
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "hub send complete")])
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-presentation-hub-send-{request_number}",
                            "hub",
                            {
                                "op": "send",
                                "to": "00000000-0000-7000-8000-000000000099",
                                "message": "ping presentation hub send",
                            },
                        )
                    ]
                )
            # Media tool cards via real `read` tool calls: the fixture seeds
            # capture.webm / hostile.svg in the workspace; the runtime
            # executes the real `read` tool and the media card renders from
            # the real result (or the unsupported-MIME rejection for
            # hostile.svg). The provider cannot construct a hostile toolResult
            # directly, so the hostile path is a real read of an unsupported
            # image MIME.
            if "presentation media video" in last_user_text(body):
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "media video complete")])
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-presentation-video-{request_number}",
                            "read",
                            {"path": "capture.webm"},
                        )
                    ]
                )
            if "presentation media hostile" in last_user_text(body):
                if last_real_message_is_tool_result(body):
                    return stream_response([text_payload(f"user-mock-{request_number}", "media hostile complete")])
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-presentation-hostile-{request_number}",
                            "read",
                            {"path": "hostile.svg"},
                        )
                    ]
                )
            # Workflow supervisor planning (web lane's workflow scenario): hand
            # it a Todo DAG so the plan commits and the workflow stays live
            # with delegated workers instead of failing planning on a
            # plain-text reply. Mirrors the dedicated `workflow` scenario;
            # ordinary steering prompts never match SUPERVISOR_RE.
            text = message_text(body)
            supervisor = SUPERVISOR_RE.search(text)
            if supervisor:
                if completed_tool_results(body, "todo"):
                    return stream_response(
                        [text_payload(f"user-mock-{request_number}", "workflow plan accepted")]
                    )
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-web-todo-{request_number}",
                            "todo",
                            todo_init([("Build", ["compile widget"]), ("Ship", ["ship widget"])]),
                        )
                    ]
                )
            # Subagents-panel e2e: the spawned child's prompt carries the
            # marker text, so stream slowly (~16s) and the job stays running
            # long enough to assert live status, message, view output, open
            # the running-job detail modal (accessibility + task/status/
            # activity + non-empty recent history + Refresh + Escape/Close),
            # and cancel deterministically. The chunk content is unchanged;
            # only the inter-chunk cadence widens the live window.
            if "web-e2e-subagent" in message_text(body):
                chunks = ["subagent-", "progress ", "step ", "one ", "complete ",
                          "auditing ", "release ", "notes ", "almost ", "done"]
                payloads = []
                for index, chunk in enumerate(chunks):
                    payloads.append(
                        {
                            "id": f"user-mock-{request_number}-{index}",
                            "model": "user-mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"content": chunk},
                                    "finish_reason": None if index < len(chunks) - 1 else "stop",
                                }
                            ],
                            "usage": {"prompt_tokens": 8, "completion_tokens": len(chunk), "total_tokens": 8 + len(chunk)},
                        }
                    )
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                slow_stream_response(self.wfile, payloads, delay=1.8)
                return None
            # Long-Chinese Task card child: the spawned DeepSeek child's system
            # prompt carries the shared Chinese CONTEXT section and its user
            # message the long-Chinese assignment, so this marker keeps the
            # child's turn streaming slowly (~18s) — the job stays live long
            # enough for the core lane to assert queued/running and cancel it.
            if "完成长中文 Task delegation 的 focused 验证" in message_text(body):
                chunks = ["长中文-", "任务 ", "验证 ", "正在 ", "渲染 ", "结构化 ", "卡片 ", "状态 ", "更新 ", "完成"]
                payloads = []
                for index, chunk in enumerate(chunks):
                    payloads.append(
                        {
                            "id": f"user-mock-{request_number}-{index}",
                            "model": "user-mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"content": chunk},
                                    "finish_reason": None if index < len(chunks) - 1 else "stop",
                                }
                            ],
                            "usage": {"prompt_tokens": 8, "completion_tokens": len(chunk), "total_tokens": 8 + len(chunk)},
                        }
                    )
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                slow_stream_response(self.wfile, payloads, delay=1.8)
                return None
            # Web coverage ToolCard driver: a main-session prompt carrying
            # this marker makes the agent call the `read` tool exactly once
            # (the second call, with the tool result, falls through to the
            # parity cadence below via completed_tool_results). Deterministic;
            # the file read is the fixture's seed.txt. Drives App.ToolCard +
            # redact.safeJson (the tool-card args pre).
            if "tool-card coverage read seed" in last_user_text(body):
                if completed_tool_results(body, "read"):
                    return stream_response(
                        [text_payload(f"user-mock-{request_number}", "seed file read complete")]
                    )
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-read-seed-{request_number}",
                            "read",
                            {"path": "seed.txt"},
                        )
                    ]
                )
            if "tool-card coverage web search" in last_user_text(body):
                if completed_tool_results(body, "web_search"):
                    return stream_response(
                        [text_payload(f"user-mock-{request_number}", "web search complete")]
                    )
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-web-search-{request_number}",
                            "web_search",
                            {
                                "query": "rpi release notes",
                                "recency": "month",
                                "limit": 3,
                                "max_tokens": 512,
                                "temperature": 0,
                                "num_search_results": 3,
                                "i": "Finding release notes",
                            },
                        )
                    ]
                )
            if "tool-card coverage edit seed" in last_user_text(body):
                if completed_tool_results(body, "edit"):
                    return stream_response(
                        [text_payload(f"user-mock-{request_number}", "edit complete")]
                    )
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-edit-seed-{request_number}",
                            "edit",
                            {
                                "path": "seed.txt",
                                "edits": [
                                    {
                                        "oldText": "web coverage seed\n",
                                        "newText": "web coverage edited\n",
                                    }
                                ],
                            },
                        )
                    ]
                )
            # Web Todo tool-card driver (coverage_test.mjs Phase P): a
            # main-session prompt with this marker makes the agent call the
            # real `todo` tool once with a phased init (two phases, two
            # tasks). The runtime executes the REAL todo tool, so the
            # transcript renders the structured Todo card: the running frame
            # projects the init args (todoPhasesFromInitArgs) and the settled
            # frame projects the result details.phases (parseTodoPhases) —
            # the same TodoToolDetails wire the TodoPanel reads.
            if "tool-card coverage todo init" in last_user_text(body):
                # Order-independent discriminator: the workflow supervisor
                # path may have called the real todo tool earlier in the same
                # session, so completed_tool_results would match that result
                # and starve this tool call. last_real_message_is_tool_result
                # distinguishes the first request (last real message = the
                # user prompt) from the follow-up (last real message = the
                # tool result).
                if last_real_message_is_tool_result(body):
                    return stream_response(
                        [text_payload(f"user-mock-{request_number}", "todo init complete")]
                    )
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-todo-init-{request_number}",
                            "todo",
                            todo_init(
                                [
                                    ("Plan", ["write the plan"]),
                                    ("Build", ["compile the widget"]),
                                ]
                            ),
                        )
                    ]
                )
            # Web tool-title branch driver (coverage_test.mjs Phase P): each
            # marker makes the model call an UNKNOWN tool whose wire name
            # exercises humanToolTitle's Title-Case / acronym-preservation /
            # credential-redaction branches through REAL tool_execution_start
            # dispatch — the backend reports "Tool <name> not found" as the
            # deterministic error result, so the card settles fast with the
            # rendered title in the DOM. Additive: no existing route changes.
            if "tool-title coverage snake" in last_user_text(body):
                if completed_tool_results(body, "code_search"):
                    return stream_response(
                        [text_payload(f"user-mock-{request_number}", "code search complete")]
                    )
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-code-search-{request_number}",
                            "code_search",
                            {"pattern": "rpi"},
                        )
                    ]
                )
            if "tool-title coverage acronym" in last_user_text(body):
                if completed_tool_results(body, "irc_rpc_status"):
                    return stream_response(
                        [text_payload(f"user-mock-{request_number}", "irc rpc complete")]
                    )
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-irc-rpc-{request_number}",
                            "irc_rpc_status",
                            {},
                        )
                    ]
                )
            if "tool-title coverage kebab" in last_user_text(body):
                if completed_tool_results(body, "url_http_check"):
                    return stream_response(
                        [text_payload(f"user-mock-{request_number}", "url http complete")]
                    )
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-url-http-{request_number}",
                            "url_http_check",
                            {"url": "https://example.org"},
                        )
                    ]
                )
            # Web Task card driver: main-session prompt asks the agent to call
            # the real `task` tool with shared context + one child. The runtime
            # executes the tool (orchestration enabled in the core fixture),
            # so the transcript renders a structured Task card.
            if "tool-card coverage task spawn" in last_user_text(body):
                if completed_tool_results(body, "task"):
                    return stream_response(
                        [text_payload(f"user-mock-{request_number}", "task spawn complete")]
                    )
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-task-card-{request_number}",
                            "task",
                            {
                                "context": (
                                    "# Goal\nShip the Task card\n\n"
                                    "# Constraints\nBe precise\n\n"
                                    "# Contract\nKeep stable ids"
                                ),
                                "tasks": [
                                    {
                                        "name": "Alpha",
                                        "agent": "writer",
                                        "task": (
                                            "web-e2e-subagent: audit the release notes "
                                            "and report findings with clear acceptance."
                                        ),
                                    }
                                ],
                            },
                        )
                    ]
                )
            # Long-Chinese Task card driver: a main-session prompt with this
            # marker makes the agent call the real `task` tool with the user's
            # full long-Chinese delegation (Goal/Constraints/Contract + one
            # DeepSeek child) so the web transcript renders the structured card
            # and the child job's live frames (queued/running/cancelled).
            if "tool-card coverage task zh" in last_user_text(body):
                # Order-independent discriminator: the ENGLISH task scenario
                # runs earlier in the same session, so completed_tool_results
                # would match its task result and starve the zh tool call.
                # last_real_message_is_tool_result distinguishes the first
                # zh request (last real message = user prompt) from the
                # follow-up (last real message = the tool result).
                if last_real_message_is_tool_result(body):
                    return stream_response(
                        [text_payload(f"user-mock-{request_number}", "task zh spawn complete")]
                    )
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-task-card-zh-{request_number}",
                            "task",
                            {
                                "context": (
                                    "# Goal\n"
                                    "验证用户给出的长中文 Task delegation（Goal/Constraints/Contract + DeepSeek child）"
                                    "在 Web transcript 中是否正确、可读、无溢出地结构化渲染。\n\n"
                                    "# Constraints\n"
                                    "只用 DeepSeek。共享工作树并发修改；不得回滚、格式化、提交或运行全套测试。"
                                    "先读现有 Task card实现与现有真实E2E；不要重复实现。默认只改测试/E2E；"
                                    "仅发现明确产品bug时，先消息Main再做最小产品修复。不得把TUI ASCII边框作为Web要求；"
                                    "Web应使用现有panel/card tokens。保留redaction/bounds/raw collapsed。\n\n"
                                    "# Contract\n"
                                    "正确渲染：标题Task；Goal/Constraints/Contract独立区块；child name/agent/target/status；"
                                    "长中文正常wrap；无水平overflow；raw JSON默认折叠；"
                                    "running→completed/cancelled状态可更新；desktop和390px mobile可读。"
                                ),
                                "tasks": [
                                    {
                                        "name": "DeepSeek",
                                        "agent": "deepseek",
                                        "task": (
                                            "完成长中文 Task delegation 的 focused 验证："
                                            "构造 Goal/Constraints/Contract + DeepSeek child 结构；"
                                            "在 Chromium 验证 desktop 与 390px mobile 的 DOM、wrap、overflow、"
                                            "raw collapsed 与状态；复用现有 core lane fixture，避免重复 lane；"
                                            "给出代码证据与真实 Chromium evidence。"
                                        ),
                                    }
                                ],
                            },
                        )
                    ]
                )
            # Code-review AI-comment markdown matrix (web code_review_paging
            # lane): the review panel submits the user's comment as a turn;
            # this marker routes the assistant review reply to a
            # markdown-rich text (bold / list / rust fence / hostile HTML
            # that must render literal). Env-gated additive branch, default
            # off — other lanes' odd/even cadence is untouched.
            if os.environ.get("MOCK_REVIEW_MARKDOWN") and "review markdown matrix" in last_user_text(body):
                review_chunks = [
                    "**review bold verdict**\n\n- review item alpha\n- review item beta\n\n",
                    "```rust\nfn review_rust() -> u32 { 42 }\n```\n\n",
                    "hostile <script>window.__crPwned=2</script><img src=x onerror=window.__crPwned=3> literal",
                ]
                if request_number % 2 == 1:
                    payloads = []
                    for index, chunk in enumerate(review_chunks):
                        payloads.append(
                            {
                                "id": f"user-mock-{request_number}-{index}",
                                "model": "user-mock",
                                "choices": [
                                    {
                                        "index": 0,
                                        "delta": {"content": chunk},
                                        "finish_reason": None if index < len(review_chunks) - 1 else "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 8, "completion_tokens": len(chunk), "total_tokens": 8 + len(chunk)},
                            }
                        )
                    self.send_response(200)
                    self.send_header("content-type", "text/event-stream")
                    self.end_headers()
                    slow_stream_response(self.wfile, payloads, delay=0.35)
                    return None
                return stream_response([text_payload(f"user-mock-{request_number}", "".join(review_chunks))])
            if request_number % 2 == 1:
                # Slow stream: several chunks over ~4s so a follow-up typed
                # while the turn is in flight lands in the queue. Chunks embed
                # the request number so the scenario can wait for THIS stream.
                chunks = [f"steer-{request_number}-", "ing ", "stream ", "chunk-", "four", "-done"]
                payloads = []
                for index, chunk in enumerate(chunks):
                    payloads.append(
                        {
                            "id": f"user-mock-{request_number}-{index}",
                            "model": "user-mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"content": chunk},
                                    "finish_reason": None if index < len(chunks) - 1 else "stop",
                                }
                            ],
                            "usage": {"prompt_tokens": 8, "completion_tokens": len(chunk), "total_tokens": 8 + len(chunk)},
                        }
                    )
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                slow_stream_response(self.wfile, payloads, delay=0.6)
                return None
            return stream_response([text_payload(f"user-mock-{request_number}", "steering-followup-reply")])
        if scenario == "bash-card":
            if request_number == 1:
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            "call-bash-card",
                            "bash",
                            {"command": BASH_CARD_COMMAND},
                        )
                    ]
                )
            if request_number == 2:
                return stream_response(
                    [text_payload(f"user-mock-{request_number}", UNCLOSED_FENCE_TEXT)]
                )
            return stream_response([text_payload(f"user-mock-{request_number}", "bash-card-extra")])
        if scenario == "todo-dag":
            if request_number == 1:
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            "call-todo-dag",
                            "todo",
                            todo_init(
                                [
                                    ("Survey", ["map parser surface", "map renderer surface"]),
                                    ("Construct", ["repair composer repaint", "bound Todo projection"]),
                                ]
                            ),
                        )
                    ]
                )
            return stream_response([text_payload(f"user-mock-{request_number}", "todo-dag-extra")])
        if scenario == "workflow":
            text = message_text(body)
            supervisor = SUPERVISOR_RE.search(text)
            if supervisor:
                if completed_tool_results(body, "todo"):
                    return stream_response(
                        [text_payload(f"user-mock-{request_number}", "workflow plan accepted")]
                    )
                # Two tool calls streamed as separate SSE deltas (one index per
                # delta), the shape streaming clients accumulate correctly.
                todo_payload = tool_call_payload(
                    f"user-mock-{request_number}",
                    f"call-workflow-todo-{request_number}",
                    "todo",
                    todo_init([("Build", ["compile widget"]), ("Ship", ["ship widget"])]),
                )
                bash_payload = tool_call_payload(
                    f"user-mock-{request_number}",
                    f"call-workflow-bash-{request_number}",
                    "bash",
                    {"command": WORKFLOW_COMMIT_COMMAND},
                )
                todo_payload["choices"][0]["delta"]["tool_calls"][0]["index"] = 0
                bash_payload["choices"][0]["delta"]["tool_calls"][0]["index"] = 1
                return stream_response([todo_payload, bash_payload])
            assignment = DELEGATED_RE.search(text)
            if assignment:
                if HOLD_WORKERS:
                    held = HeldStream(
                        self.wfile,
                        request_number,
                        "",
                        f"worker completed: {assignment.group(1)}",
                        WORKER_RELEASE,
                    )
                    HELD_STREAMS.append(held)
                    return held
                return stream_response(
                    [text_payload(f"user-mock-{request_number}", f"worker completed: {assignment.group(1)}")]
                )
            return stream_response([text_payload(f"user-mock-{request_number}", "workflow-extra-reply")])
        if scenario == "sessions":
            # Multi-session web lane (E2E.d/web/sessions_test.mjs): content-
            # routed, because the mock's request counter is GLOBAL across the
            # concurrently-running session runtimes — request numbers
            # interleave nondeterministically, so replies are keyed by the
            # prompt text, never by request number. The markers are matched
            # EXACTLY against the current prompt (never the history): the
            # session context accumulates past turns, and "sessions-slow-a"
            # is a substring of "sessions-slow-a2" — substring matching
            # against history would misroute the second slow stream.
            #
            #   "sessions-slow-a"   slow stream (~10s) tail "slow-a-done"
            #   "sessions-slow-a2"  slow stream (~10s) tail "slow-a2-done"
            #   "sessions-slow-b"   slow stream (~8s)  tail "slow-b-done"
            #   "sessions-slow-b3"  first delta, held until its release POST
            #   supervisor planning -> todo init + acceptance (workflow lane)
            #   anything else       instant echo "sessions-reply: <prompt>"
            text = message_text(body)
            user = last_user_text(body).strip()
            if user == "sessions-slow-b3":
                held = HeldStream(
                    self.wfile,
                    request_number,
                    "sessions-slow-b3-1/",
                    "slow-b3-done",
                    SESSION_RELEASE,
                )
                HELD_STREAMS.append(held)
                return held
            for marker, tail, delay, chunks in (
                # specific markers FIRST: "sessions-slow-a2" would otherwise
                # match the "sessions-slow-a" branch (substring), and b3/b.
                ("sessions-slow-a2", "slow-a2-done", 0.7, 14),
                ("sessions-slow-a", "slow-a-done", 0.7, 14),
                # b3 is handled by the deterministic held-stream branch above.
                ("sessions-slow-b", "slow-b-done", 0.6, 12),
            ):
                if user == marker:
                    payloads = []
                    for index in range(chunks):
                        finish = "stop" if index == chunks - 1 else None
                        chunk = f"{marker}-{index + 1}/" if index < chunks - 1 else tail
                        payloads.append(
                            {
                                "id": f"user-mock-{request_number}-{index}",
                                "model": "user-mock",
                                "choices": [
                                    {
                                        "index": 0,
                                        "delta": {"content": chunk},
                                        "finish_reason": finish,
                                    }
                                ],
                                "usage": {"prompt_tokens": 8, "completion_tokens": len(chunk), "total_tokens": 8 + len(chunk)},
                            }
                        )
                    self.send_response(200)
                    self.send_header("content-type", "text/event-stream")
                    self.end_headers()
                    slow_stream_response(self.wfile, payloads, delay=delay)
                    return None
            supervisor = SUPERVISOR_RE.search(text)
            if supervisor:
                if completed_tool_results(body, "todo"):
                    return stream_response(
                        [text_payload(f"user-mock-{request_number}", "workflow plan accepted")]
                    )
                return stream_response(
                    [
                        tool_call_payload(
                            f"user-mock-{request_number}",
                            f"call-sessions-todo-{request_number}",
                            "todo",
                            todo_init([("Build", ["compile widget"]), ("Ship", ["ship widget"])]),
                        )
                    ]
                )
            return stream_response(
                [text_payload(f"user-mock-{request_number}", f"sessions-reply: {user}")]
            )
        if scenario == "scroll":
            # Web transcript scroll-pinning lane: content-routed, exact prompt
            # text only (the request counter is global, so never route by
            # number). "scroll-long-a" is a long slow stream; "scroll-echo"
            # is an instant echo for the second session's round-trip.
            user = last_user_text(body).strip()
            if user == "scroll-long-a":
                payloads = []
                for index in range(SCROLL_LONG_CHUNKS):
                    marker = "scroll-long-a-done" if index == SCROLL_LONG_CHUNKS - 1 else f"scroll-long-a-{index:02d}"
                    chunk = f"{marker}: {SCROLL_LONG_FILLER}"
                    payloads.append(
                        {
                            "id": f"user-mock-{request_number}-{index}",
                            "model": "user-mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"content": chunk},
                                    "finish_reason": "stop" if index == SCROLL_LONG_CHUNKS - 1 else None,
                                }
                            ],
                            "usage": {"prompt_tokens": 8, "completion_tokens": len(chunk), "total_tokens": 8 + len(chunk)},
                        }
                    )
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                slow_stream_response(self.wfile, payloads, delay=SCROLL_LONG_DELAY)
                return None
            if user == "scroll-echo":
                return stream_response([text_payload(f"user-mock-{request_number}", "scroll-echo-reply")])
            if user == "scroll-final-md":
                # Streams the raw markdown in paragraph-sized deltas; the last
                # delta carries finish_reason "stop" so the provider assembles
                # the full text and message_end commits the rendered blocks.
                payloads = []
                for index, part in enumerate(SCROLL_FINAL_MD_PARTS):
                    payloads.append(
                        {
                            "id": f"user-mock-{request_number}-md-{index}",
                            "model": "user-mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"content": part},
                                    "finish_reason": "stop" if index == len(SCROLL_FINAL_MD_PARTS) - 1 else None,
                                }
                            ],
                            "usage": {"prompt_tokens": 8, "completion_tokens": len(part), "total_tokens": 8 + len(part)},
                        }
                    )
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                slow_stream_response(self.wfile, payloads, delay=SCROLL_MD_DELAY)
                return None
            if user == "scroll-narrow":
                payloads = []
                for index in range(SCROLL_NARROW_CHUNKS):
                    marker = "scroll-narrow-done" if index == SCROLL_NARROW_CHUNKS - 1 else f"scroll-narrow-{index:02d}"
                    chunk = f"{marker}: {SCROLL_NARROW_FILLER}"
                    payloads.append(
                        {
                            "id": f"user-mock-{request_number}-narrow-{index}",
                            "model": "user-mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"content": chunk},
                                    "finish_reason": "stop" if index == SCROLL_NARROW_CHUNKS - 1 else None,
                                }
                            ],
                            "usage": {"prompt_tokens": 8, "completion_tokens": len(chunk), "total_tokens": 8 + len(chunk)},
                        }
                    )
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                slow_stream_response(self.wfile, payloads, delay=SCROLL_NARROW_DELAY)
                return None
            return stream_response([text_payload(f"user-mock-{request_number}", f"scroll-unrouted: {user}")])
        if scenario == "slowclient":
            # Slow-client web lane (E2E.d/web/slowclient.sh ->
            # slowclient_test.mjs): content-routed by EXACT prompt text (the
            # request counter is global across concurrent sessions, so never
            # route by number). Prompts:
            #   slowclient-burst-<N>-<bytes>          N deltas, back-to-back
            #   slowclient-burstslow-<N>-<bytes>-<d>  N deltas at <d>s cadence
            #   slowclient-heavy-<count>              one heavy final message
            user = last_user_text(body).strip()
            burst = re.fullmatch(r"slowclient-burst-(\d+)-(\d+)", user)
            if burst:
                count = int(burst.group(1))
                size = int(burst.group(2))
                prefix = f"s{count}:"
                payloads = []
                for index in range(count):
                    chunk = f"slowclient-done-{count}" if index == count - 1 else f"{prefix}{index}/"
                    if len(chunk) < size:
                        chunk = chunk + "x" * (size - len(chunk))
                    payloads.append(
                        {
                            "id": f"user-mock-{request_number}-{index}",
                            "model": "user-mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"content": chunk},
                                    "finish_reason": "stop" if index == count - 1 else None,
                                }
                            ],
                            "usage": {"prompt_tokens": 8, "completion_tokens": len(chunk), "total_tokens": 8 + len(chunk)},
                        }
                    )
                return stream_response(payloads)
            burstslow = re.fullmatch(r"slowclient-burstslow-(\d+)-(\d+)-([0-9.]+)", user)
            if burstslow:
                count = int(burstslow.group(1))
                size = int(burstslow.group(2))
                delay = float(burstslow.group(3))
                prefix = f"s{count}:"
                payloads = []
                for index in range(count):
                    chunk = f"slowclient-done-{count}" if index == count - 1 else f"{prefix}{index}/"
                    if len(chunk) < size:
                        chunk = chunk + "x" * (size - len(chunk))
                    payloads.append(
                        {
                            "id": f"user-mock-{request_number}-{index}",
                            "model": "user-mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"content": chunk},
                                    "finish_reason": "stop" if index == count - 1 else None,
                                }
                            ],
                            "usage": {"prompt_tokens": 8, "completion_tokens": len(chunk), "total_tokens": 8 + len(chunk)},
                        }
                    )
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                burst_stream_response(self.wfile, payloads)
                return None
            heavy = re.fullmatch(r"slowclient-heavy-(\d+)", user)
            if heavy:
                count = int(heavy.group(1))
                parts = []
                for index in range(count):
                    parts.append(
                        f"### Heavy {index}\n\n"
                        f"Inline math $x_{{{index}}}^{{{index + 1}}}$ and a display block:\n\n"
                        f"$$\\sum_{{k=1}}^{{{index + 1}}} \\frac{{k}}{{k+1}} = "
                        f"\\frac{{{index + 1} \\cdot {index + 2}}}{{2}}$$\n\n"
                    )
                mermaid_edges = "\n".join(f"A{i}-->A{i + 1}" for i in range(count - 1))
                heavy_text = (
                    "".join(parts)
                    + "A diagram:\n\n```mermaid\ngraph LR;\n"
                    + mermaid_edges
                    + "\n```\n\n"
                    + "A table:\n\n| i | i^2 |\n|---|---|\n"
                    + "".join(f"| {i} | {i * i} |\n" for i in range(count))
                    + "slowclient-heavy-done"
                )
                return stream_response([text_payload(f"user-mock-{request_number}", heavy_text)])
            return stream_response([text_payload(f"user-mock-{request_number}", f"slowclient-unrouted: {user}")])
        return stream_response([text_payload(f"user-mock-{request_number}", "unrouted")])

    def log_message(self, _format: str, *_args: object) -> None:
        return


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--scenario", required=True, choices=["steering", "bash-card", "todo-dag", "workflow", "xss", "sessions", "scroll", "slowclient"])
    parser.add_argument("--port-file", required=True)
    parser.add_argument(
        "--hold-workers",
        action="store_true",
        help="workflow scenario only: hold worker completion streams until POST /__release",
    )
    args = parser.parse_args()
    global HOLD_WORKERS
    HOLD_WORKERS = args.hold_workers
    WORKER_RELEASE.clear()
    SESSION_RELEASE.clear()
    HELD_STREAMS.clear()
    UserMockServer.serial = 0
    UserMockServer.scenario = args.scenario
    # Threaded: held streams wait while the control endpoint accepts their
    # release POST. Other scenario requests also retain concurrency headroom.
    server = ThreadingHTTPServer(("127.0.0.1", 0), UserMockServer)
    with open(args.port_file, "w", encoding="utf-8") as handle:
        handle.write(str(server.server_address[1]))
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
