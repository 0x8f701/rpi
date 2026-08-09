#!/usr/bin/env python3
"""Drive real rpi/tmux through Main→Alpha→Beta→Main hub AgentTool calls."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

MAIN_TO_ALPHA = "main-to-alpha-tmux"
ALPHA_TO_BETA = "alpha-to-beta-tmux"
BETA_TO_MAIN = "beta-to-main-tmux"
ALPHA_SENTINEL = "ALPHA_CHILD_OWNED_HUB_SENTINEL"
BETA_SENTINEL = "BETA_CHILD_OWNED_HUB_SENTINEL"


def stream_response(payload: dict[str, Any]) -> bytes:
    return f"data: {json.dumps(payload, separators=(',', ':'))}\n\ndata: [DONE]\n\n".encode()


def tool_response(route: str, call_id: str, name: str, arguments: dict[str, Any]) -> bytes:
    return stream_response(
        {
            "id": f"hub-tui-{route}",
            "model": "mock",
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
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        }
    )


def text_response(route: str, text: str) -> bytes:
    return stream_response(
        {
            "id": f"hub-tui-{route}",
            "model": "mock",
            "choices": [
                {"index": 0, "delta": {"content": text}, "finish_reason": "stop"}
            ],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        }
    )


def tool_names(body: dict[str, Any]) -> set[str]:
    return {
        item.get("function", {}).get("name", "")
        for item in body.get("tools", [])
        if item.get("type") == "function"
    }


def has_tool_result(body: dict[str, Any], call_id: str) -> bool:
    return any(
        message.get("role") == "tool" and message.get("tool_call_id") == call_id
        for message in body.get("messages", [])
    )


class HubTuiHandler(BaseHTTPRequestHandler):
    lock = threading.Lock()
    states = {"main": 0, "alpha": 0, "beta": 0}
    transitions: list[str] = []
    request_error: BaseException | None = None
    requests: list[dict[str, Any]] = []
    completed = threading.Event()
    alpha_wait_issued = threading.Event()
    beta_wait_issued = threading.Event()
    main_send_completed = threading.Event()
    alpha_send_completed = threading.Event()

    def do_POST(self) -> None:
        try:
            length = int(self.headers.get("content-length", "0"))
            body = json.loads(self.rfile.read(length))
            serialized = json.dumps(body, separators=(",", ":"))
            route = "alpha" if ALPHA_SENTINEL in serialized else "beta" if BETA_SENTINEL in serialized else "main"
            type(self).requests.append({"route": route, "body": body})
            names = tool_names(body)
            if route == "main" and not {"task", "hub"}.issubset(names):
                raise AssertionError(f"Main request omitted real task/hub tools: {sorted(names)!r}")
            if route != "main" and "hub" not in names:
                raise AssertionError(f"{route} request omitted child-owned hub tool: {sorted(names)!r}")

            with type(self).lock:
                state = type(self).states[route]
                type(self).states[route] = state + 1
                type(self).transitions.append(f"{route}:{state}")

            if route == "main" and state == 0:
                response = tool_response(
                    route,
                    "main-spawn-alpha-beta",
                    "task",
                    {
                        "context": "Hub relay contract: Main coordinates, Alpha relays to Beta, Beta relays to Main; every message body is passed through unchanged.",
                        "tasks": [
                            {"name": "Alpha", "agent": "Alpha", "task": "Wait for Main, then send Beta."},
                            {"name": "Beta", "agent": "Beta", "task": "Wait for Alpha, then send Main."},
                        ]
                    },
                )
            elif route == "main" and state == 1:
                if not has_tool_result(body, "main-spawn-alpha-beta"):
                    raise AssertionError("Main did not receive the real task tool result")
                if not type(self).alpha_wait_issued.wait(5) or not type(self).beta_wait_issued.wait(5):
                    raise AssertionError("children did not issue their hub waits before Main send")
                response = tool_response(route, "main-send-alpha", "hub", {"op": "send", "to": "Alpha", "message": MAIN_TO_ALPHA})
            elif route == "main" and state == 2:
                if not has_tool_result(body, "main-send-alpha"):
                    raise AssertionError("Main did not receive its hub send result")
                response = tool_response(route, "main-wait-beta", "hub", {"op": "wait", "from": "Beta", "timeoutMs": 10000})
            elif route == "main" and state == 3:
                if not has_tool_result(body, "main-wait-beta") or BETA_TO_MAIN not in serialized:
                    raise AssertionError("Main hub wait did not receive Beta's exact body")
                response = text_response(route, "hub tool campaign complete")
                type(self).completed.set()
            elif route == "alpha" and state == 0:
                type(self).alpha_wait_issued.set()
                response = tool_response(route, "alpha-wait-main", "hub", {"op": "wait", "from": "Main", "timeoutMs": 10000})
            elif route == "alpha" and state == 1:
                if not has_tool_result(body, "alpha-wait-main") or MAIN_TO_ALPHA not in serialized:
                    raise AssertionError("Alpha hub wait did not receive Main's exact body")
                if not type(self).beta_wait_issued.wait(5):
                    raise AssertionError("Beta did not issue its hub wait before Alpha send")
                response = tool_response(route, "alpha-send-beta", "hub", {"op": "send", "to": "Beta", "message": ALPHA_TO_BETA})
            elif route == "alpha" and state == 2:
                if not has_tool_result(body, "alpha-send-beta"):
                    raise AssertionError("Alpha did not receive its owned hub send result")
                response = text_response(route, "Alpha hub relay complete")
            elif route == "beta" and state == 0:
                type(self).beta_wait_issued.set()
                response = tool_response(route, "beta-wait-alpha", "hub", {"op": "wait", "from": "Alpha", "timeoutMs": 10000})
            elif route == "beta" and state == 1:
                if not has_tool_result(body, "beta-wait-alpha") or ALPHA_TO_BETA not in serialized:
                    raise AssertionError("Beta hub wait did not receive Alpha's exact body")
                response = tool_response(route, "beta-send-main", "hub", {"op": "send", "to": "Main", "message": BETA_TO_MAIN})
            elif route == "beta" and state == 2:
                if not has_tool_result(body, "beta-send-main"):
                    raise AssertionError("Beta did not receive its owned hub send result")
                response = text_response(route, "Beta hub relay complete")
            else:
                raise AssertionError(f"unexpected provider request route={route} state={state}")
        except BaseException as error:
            type(self).request_error = error
            type(self).completed.set()
            response = text_response("error", "hub mock failed")

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


def wait_for(session: str, needles: list[str], timeout: float) -> str:
    deadline = time.monotonic() + timeout
    latest = ""
    while time.monotonic() < deadline:
        latest = capture(session)
        if all(needle in latest for needle in needles):
            return latest
        if HubTuiHandler.request_error is not None:
            raise HubTuiHandler.request_error
        time.sleep(0.1)
    raise AssertionError(f"TUI did not display {needles!r}; final pane:\n{latest}")


def write_agent(path: Path, name: str, sentinel: str) -> None:
    path.write_text(
        f"---\nname: {name}\ndescription: deterministic hub child\ntools:\n  - hub\n---\n{sentinel}\nUse only the hub calls supplied by the localhost provider.\n",
        encoding="utf-8",
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rpi", required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--evidence", required=True)
    args = parser.parse_args()

    home = Path(args.home).resolve()
    workspace = Path(args.workspace).resolve()
    evidence = Path(args.evidence).resolve()
    rpi = str(Path(args.rpi).resolve())
    agent_dir = home / ".pi" / "agent"
    agents_dir = agent_dir / "agents"
    agents_dir.mkdir(parents=True, exist_ok=True)
    workspace.mkdir(parents=True, exist_ok=True)
    evidence.mkdir(parents=True, exist_ok=True)
    (agent_dir / "settings.json").write_text(
        json.dumps({"orchestration": {"tasks": True, "todo": False, "maxConcurrency": 2, "maxRecursionDepth": 2}}),
        encoding="utf-8",
    )
    write_agent(agents_dir / "alpha.md", "Alpha", ALPHA_SENTINEL)
    write_agent(agents_dir / "beta.md", "Beta", BETA_SENTINEL)

    server = ThreadingHTTPServer(("127.0.0.1", 0), HubTuiHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    port = server.server_address[1]
    (agent_dir / "models.json").write_text(
        json.dumps(
            {
                "providers": {
                    "hub-tui-e2e": {
                        "baseUrl": f"http://127.0.0.1:{port}",
                        "api": "openai-completions",
                        "models": [{"id": "mock", "name": "Hub TUI Mock", "contextWindow": 32768, "maxTokens": 2048}],
                    }
                }
            }
        ),
        encoding="utf-8",
    )
    session = f"rpi-e2e-hub-tui-{os.getpid()}"
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
    try:
        run_tmux(
            "new-session", "-d", "-s", session, "-x", "120", "-y", "40", "-c", str(workspace),
            "env", *environment, rpi, "--model", "hub-tui-e2e/mock", "--api-key", "localhost-hub-e2e-not-a-secret",
        )
        time.sleep(1.5)
        run_tmux("send-keys", "-t", f"{session}:0", "-l", "Run the authoritative child-owned hub campaign.")
        run_tmux("send-keys", "-t", f"{session}:0", "Enter")
        pane = wait_for(session, ["hub tool campaign complete", "IRC · Beta → Main", BETA_TO_MAIN], 35)
        pane = wait_for(session, ["Alpha (Alpha) · completed", "Beta (Beta) · completed"], 10)
        if HubTuiHandler.request_error is not None:
            raise HubTuiHandler.request_error
        expected_states = {"main": 4, "alpha": 3, "beta": 3}
        if HubTuiHandler.states != expected_states or not HubTuiHandler.completed.is_set():
            raise AssertionError(f"incomplete provider route states: {HubTuiHandler.states!r}")
        if "<orchestration-message" in pane:
            raise AssertionError("raw orchestration XML leaked into public TUI")
        (evidence / "tui.txt").write_text(pane, encoding="utf-8")
        (evidence / "provider-transitions.json").write_text(
            json.dumps({"states": HubTuiHandler.states, "transitions": HubTuiHandler.transitions}, indent=2),
            encoding="utf-8",
        )
        (evidence / "assertions.json").write_text(
            json.dumps(
                {
                    "status": "passed",
                    "checks": [
                        "real-task-tool-spawn",
                        "main-parent-hub-send",
                        "alpha-child-owned-hub-wait-send",
                        "beta-child-owned-hub-wait-send",
                        "visible-beta-to-main-irc",
                        "alpha-beta-completed-jobs",
                    ],
                }
            ),
            encoding="utf-8",
        )
    except BaseException:
        (evidence / "failure-tui.txt").write_text(capture(session), encoding="utf-8")
        (evidence / "failure-provider.json").write_text(
            json.dumps({"states": HubTuiHandler.states, "transitions": HubTuiHandler.transitions, "requests": HubTuiHandler.requests}, indent=2),
            encoding="utf-8",
        )
        raise
    finally:
        run_tmux("kill-session", "-t", session, check=False)
        server.shutdown()
        thread.join(timeout=5)
        server.server_close()

    print(json.dumps({"status": "passed", "evidence": str(evidence)}))


if __name__ == "__main__":
    main()
