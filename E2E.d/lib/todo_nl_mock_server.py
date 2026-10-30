#!/usr/bin/env python3
"""Loopback OpenAI-compatible mock for the natural-language Todo lane.

Content-routed by exact prompt text (the counter is global, so never route by
number): each NL prompt maps to ONE typed `todo` tool call; once a `todo` tool
result follows the prompt, follow-ups reply plain text so the turn settles.
"""

from __future__ import annotations

import argparse
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

TODO_NL_PREFIX = "todo nl: "

TODO_MUTATIONS: dict[str, dict[str, Any]] = {
    "add a task": {
        "op": "append",
        "phase": "Delivery",
        "items": ["send the welcome email"],
        "list": None,
        "task": None,
        "dependsOn": None,
        "cascade": None,
    },
    "remove the added task": {
        "op": "rm",
        "task": "send the welcome email",
        "phase": None,
        "cascade": False,
        "list": None,
        "items": None,
        "dependsOn": None,
    },
    "remove the missing task": {
        "op": "rm",
        "task": "never created task 6f21",
        "phase": None,
        "cascade": False,
        "list": None,
        "items": None,
        "dependsOn": None,
    },
    "clear the list": {
        "op": "rm",
        "task": None,
        "phase": None,
        "cascade": None,
        "list": None,
        "items": None,
        "dependsOn": None,
    },
}


def stream_response(payloads: list[dict[str, Any]]) -> bytes:
    rows = [f"data: {json.dumps(payload, separators=(',', ':'))}\n\n" for payload in payloads]
    rows.append("data: [DONE]\n\n")
    return "".join(rows).encode()


def tool_call_payload(rid: str, call_id: str, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": rid,
        "model": "todo-nl-mock",
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
        "model": "todo-nl-mock",
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 8, "completion_tokens": len(text), "total_tokens": 8 + len(text)},
    }


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


def last_user_text(body: dict[str, Any]) -> str:
    for message in reversed(body.get("messages") or []):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, str):
            text = content
        elif isinstance(content, list):
            text = "\n".join(
                block.get("text", "")
                for block in content
                if isinstance(block, dict) and isinstance(block.get("text"), str)
            )
        else:
            continue
        stripped = text.lstrip()
        if stripped.startswith("<system-reminder>") or stripped.startswith("<orchestration-message"):
            continue
        return text
    return ""



def last_user_index(body: dict[str, Any]) -> int:
    for index in range(len(body.get("messages") or []) - 1, -1, -1):
        message = body["messages"][index]
        if message.get("role") == "user":
            content = message.get("content")
            text = content if isinstance(content, str) else message_text({"messages": [message]})
            stripped = text.lstrip()
            if stripped.startswith("<system-reminder>") or stripped.startswith("<orchestration-message"):
                continue
            return index
    return -1


def route(request_number: int, body: dict[str, Any]) -> bytes:
    messages = body.get("messages") or []
    user_index = last_user_index(body)
    pending_tool_result = any(
        message.get("role") == "tool" for message in messages[user_index + 1 :]
    )
    prompt = last_user_text(body).strip()
    if pending_tool_result:
        return stream_response(
            [text_payload(f"todo-nl-mock-{request_number}", f"nl-mutations-ack-{request_number}")]
        )
    for needle, arguments in TODO_MUTATIONS.items():
        if prompt == f"{TODO_NL_PREFIX}{needle}":
            return stream_response(
                [
                    tool_call_payload(
                        f"todo-nl-mock-{request_number}",
                        f"call-todo-nl-{request_number}",
                        "todo",
                        arguments,
                    )
                ]
            )
    return stream_response(
        [text_payload(f"todo-nl-mock-{request_number}", f"todo-nl-unrouted: {prompt}")]
    )


class TodoNlMockServer(BaseHTTPRequestHandler):
    serial = 0

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        try:
            body = json.loads(raw.decode("utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError):
            body = {}
        TodoNlMockServer.serial += 1
        request_number = TodoNlMockServer.serial
        print(
            f"todo-nl-mock request#{request_number} "
            f"user_len={len(last_user_text(body))}",
            file=sys.stderr,
            flush=True,
        )
        payload = route(request_number, body)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        self.wfile.write(payload)
        self.wfile.flush()

    def log_message(self, format: str, *args: Any) -> None:
        return


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port-file", required=True)
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", 0), TodoNlMockServer)
    with open(args.port_file, "w", encoding="utf-8") as handle:
        handle.write(str(server.server_address[1]))
    server.serve_forever()


if __name__ == "__main__":
    main()