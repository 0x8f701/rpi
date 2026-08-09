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
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import threading
import time
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
# flowchart fence, inline $...$ math and a $$...$$ display block, plus (for
# renderer coverage) an ordinary ```text code fence, a safe http link, a
# blocked javascript link, a safe data:image PNG, and a blocked data:text
# image. Raw string so backslashes in the LaTeX survive; returned only when
# the prompt asks for "render rich content" (never by request number, so
# other steering lanes keep their odd/even slow/instant cadence untouched).
# The fence/links/images render inert (no raw markers in textContent) and
# only ADD observable elements, so the existing assertions still hold.
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

Safe link [pi web docs](https://example.org/pi-web) and a blocked
[evil](javascript:alert(1)) link that must not become a live anchor.

Safe image ![pic](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=)
and a blocked ![x](data:text/html,<b>hi</b>) image that must stay inert.
"""


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
            # marker text, so stream slowly (~7s) and the job stays running
            # long enough to assert live status, message, view output, and
            # cancel deterministically.
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
                slow_stream_response(self.wfile, payloads, delay=0.7)
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
        return stream_response([text_payload(f"user-mock-{request_number}", "unrouted")])

    def log_message(self, _format: str, *_args: object) -> None:
        return


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--scenario", required=True, choices=["steering", "bash-card", "todo-dag", "workflow", "xss", "sessions"])
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
