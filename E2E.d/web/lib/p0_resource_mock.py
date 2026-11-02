#!/usr/bin/env python3
"""Self-contained OpenAI-completions mock for the P0 resource harness.

Does not import or modify E2E.d/lib/user_mock_server.py. Routes by the last
real user prompt:

  p0-slow-a   short slow SSE (background-stream overlap)
  anything else  instant echo "p0-reply: <prompt>"
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


def last_user_text(body: dict[str, Any]) -> str:
    messages = body.get("messages") or []
    for message in reversed(messages):
        if not isinstance(message, dict) or message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, str):
            text = content.strip()
            if text and not text.lstrip().startswith("<"):
                return text
            continue
        if isinstance(content, list):
            parts: list[str] = []
            for part in content:
                if isinstance(part, dict) and part.get("type") == "text":
                    parts.append(str(part.get("text") or ""))
                elif isinstance(part, str):
                    parts.append(part)
            text = "\n".join(parts).strip()
            if text and not text.lstrip().startswith("<"):
                return text
    return ""


def text_payload(rid: str, text: str, finish: str | None = "stop") -> dict[str, Any]:
    return {
        "id": rid,
        "model": "user-mock",
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": finish}],
        "usage": {
            "prompt_tokens": 8,
            "completion_tokens": len(text),
            "total_tokens": 8 + len(text),
        },
    }


def write_sse(handler: BaseHTTPRequestHandler, payloads: list[dict[str, Any]], delay: float = 0.0) -> None:
    handler.send_response(200)
    handler.send_header("content-type", "text/event-stream")
    handler.end_headers()
    for index, payload in enumerate(payloads):
        if index > 0 and delay > 0:
            time.sleep(delay)
        handler.wfile.write(f"data: {json.dumps(payload, separators=(',', ':'))}\n\n".encode())
        handler.wfile.flush()
    handler.wfile.write(b"data: [DONE]\n\n")
    handler.wfile.flush()


class P0ResourceMock(BaseHTTPRequestHandler):
    serial = 0

    def log_message(self, format: str, *args: object) -> None:
        sys.stderr.write("p0-mock: " + (format % args) + "\n")

    def do_GET(self) -> None:
        body = b'{"object":"list","data":[{"id":"mock"}]}\n'
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self) -> None:
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw)
        except json.JSONDecodeError:
            body = {}
        type(self).serial += 1
        request_number = type(self).serial
        user = last_user_text(body).strip() if isinstance(body, dict) else ""
        if user == "p0-slow-a":
            chunks = 6
            payloads = []
            for index in range(chunks):
                finish = "stop" if index == chunks - 1 else None
                text = f"p0-slow-a-{index + 1}/" if index < chunks - 1 else "slow-a-done"
                payloads.append(text_payload(f"p0-mock-{request_number}-{index}", text, finish))
            write_sse(self, payloads, delay=0.12)
            return
        reply = f"p0-reply: {user}" if user else "p0-reply:"
        write_sse(self, [text_payload(f"p0-mock-{request_number}", reply)])


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port-file", required=True)
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", 0), P0ResourceMock)
    with open(args.port_file, "w", encoding="utf-8") as handle:
        handle.write(str(server.server_address[1]))
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
