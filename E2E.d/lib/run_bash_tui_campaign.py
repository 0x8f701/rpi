#!/usr/bin/env python3
"""Verify foreground Bash cannot claim the interactive TUI terminal."""

from __future__ import annotations

import argparse
import json
import os
import secrets
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from typing import Any


BASH_COMMAND = (
    "printf 'Username for mock: ' >&2; "
    "IFS= read -r line; read_status=$?; "
    "printf 'stdin-closed:%s git-prompt:%s gh-prompt:%s pager:%s\\n' "
    '"$read_status" "$GIT_TERMINAL_PROMPT" "$GH_PROMPT_DISABLED" "$PAGER"'
)


def stream_response(payloads: list[dict[str, Any]]) -> bytes:
    rows = [f"data: {json.dumps(payload, separators=(',', ':'))}\n\n" for payload in payloads]
    rows.append("data: [DONE]\n\n")
    return "".join(rows).encode()


class BashTuiHandler(BaseHTTPRequestHandler):
    request_count = 0
    request_error: BaseException | None = None
    completed = threading.Event()

    def do_POST(self) -> None:
        try:
            length = int(self.headers.get("content-length", "0"))
            body = json.loads(self.rfile.read(length))
            type(self).request_count += 1
            request_number = type(self).request_count
            if request_number == 1:
                arguments = json.dumps({"command": BASH_COMMAND}, separators=(",", ":"))
                response = stream_response(
                    [
                        {
                            "id": "bash-tui-1",
                            "model": "mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {
                                        "tool_calls": [
                                            {
                                                "index": 0,
                                                "id": "call-bash-tui",
                                                "type": "function",
                                                "function": {"name": "bash", "arguments": arguments},
                                            }
                                        ]
                                    },
                                    "finish_reason": "tool_calls",
                                }
                            ],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                        }
                    ]
                )
            else:
                serialized = json.dumps(body, separators=(",", ":"))
                for expected in ("stdin-closed:1", "git-prompt:0", "gh-prompt:1", "pager:cat"):
                    if expected not in serialized:
                        raise AssertionError(f"Bash result omitted unattended marker: {expected}")
                response = stream_response(
                    [
                        {
                            "id": "bash-tui-2",
                            "model": "mock",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"content": "mock complete"},
                                    "finish_reason": "stop",
                                }
                            ],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                        }
                    ]
                )
                type(self).completed.set()
        except BaseException as error:
            type(self).request_error = error
            type(self).completed.set()
            response = stream_response(
                [
                    {
                        "id": "bash-tui-error",
                        "model": "mock",
                        "choices": [{"index": 0, "delta": {"content": "mock failed"}, "finish_reason": "stop"}],
                    }
                ]
            )

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)

    def log_message(self, _format: str, *_args: object) -> None:
        return


def run_tmux(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["tmux", *args], text=True, capture_output=True, check=check)


def capture(session: str) -> str:
    return run_tmux("capture-pane", "-p", "-S", "-2000", "-t", f"{session}:0").stdout


def wait_for(session: str, needle: str, timeout: float) -> str:
    deadline = time.monotonic() + timeout
    latest = ""
    while time.monotonic() < deadline:
        latest = capture(session)
        if needle in latest:
            return latest
        time.sleep(0.1)
    raise AssertionError(f"TUI did not display {needle!r}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rpi", required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--evidence", required=True)
    args = parser.parse_args()

    home = Path(args.home)
    workspace = Path(args.workspace)
    evidence = Path(args.evidence)
    rpi = str(Path(args.rpi).resolve())
    agent_dir = home / ".pi" / "agent"
    agent_dir.mkdir(parents=True, exist_ok=True)
    workspace.mkdir(parents=True, exist_ok=True)
    evidence.mkdir(parents=True, exist_ok=True)

    server = HTTPServer(("localhost", 0), BashTuiHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    port = server.server_address[1]
    (agent_dir / "models.json").write_text(
        json.dumps(
            {
                "providers": {
                    "bash-tui-e2e": {
                        "baseUrl": f"http://localhost:{port}",
                        "api": "openai-completions",
                        "models": [
                            {
                                "id": "mock",
                                "name": "Bash TUI Mock",
                                "contextWindow": 32768,
                                "maxTokens": 2048,
                            }
                        ],
                    }
                }
            }
        ),
        encoding="utf-8",
    )

    session = f"rpi-e2e-bash-tui-{os.getpid()}"
    environment = [
        f"HOME={home}",
        f"USERPROFILE={home}",
        f"PATH={os.environ.get('PATH', '')}",
        f"LANG={os.environ.get('LANG', 'C.UTF-8')}",
        f"LC_ALL={os.environ.get('LC_ALL', 'C.UTF-8')}",
        f"PI_CODING_AGENT_DIR={agent_dir}",
        "PI_SKIP_VERSION_CHECK=1",
        "TERM=xterm-256color",
    ]
    command = [
        "new-session",
        "-d",
        "-s",
        session,
        "-x",
        "120",
        "-y",
        "40",
        "-c",
        str(workspace),
        "env",
        *environment,
        rpi,
        "--model",
        "bash-tui-e2e/mock",
        "--api-key",
        secrets.token_urlsafe(24),
    ]
    try:
        run_tmux(*command)
        time.sleep(1.5)
        run_tmux("send-keys", "-t", f"{session}:0", "-l", "Run the finite Bash ownership check.")
        run_tmux("send-keys", "-t", f"{session}:0", "Enter")
        try:
            pane = wait_for(session, "mock complete", 30)
        except BaseException:
            (evidence / "timeout.txt").write_text(capture(session), encoding="utf-8")
            raise
        if not BashTuiHandler.completed.is_set() or BashTuiHandler.request_count < 2:
            raise AssertionError("mock did not receive the completed Bash tool result")
        if BashTuiHandler.request_error is not None:
            raise BashTuiHandler.request_error

        sentinel = "BASH-COMPOSER-SENTINEL"
        run_tmux("send-keys", "-t", f"{session}:0", "-l", sentinel)
        pane = wait_for(session, sentinel, 5)
        run_tmux("send-keys", "-t", f"{session}:0", "-l", "X")
        pane = wait_for(session, f"{sentinel}X", 5)
        (evidence / "tui.txt").write_text(pane, encoding="utf-8")
        (evidence / "assertions.json").write_text(
            json.dumps(
                {
                    "status": "passed",
                    "checks": ["stdin-closed", "unattended-environment", "turn-completed", "composer-editable"],
                }
            ),
            encoding="utf-8",
        )
    finally:
        run_tmux("kill-session", "-t", session, check=False)
        server.shutdown()
        thread.join(timeout=5)
        server.server_close()

    print(json.dumps({"status": "passed", "evidence": str(evidence)}))


if __name__ == "__main__":
    main()
