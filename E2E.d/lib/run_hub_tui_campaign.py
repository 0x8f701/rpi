#!/usr/bin/env python3
"""Drive real rpi/tmux through Main→Alpha→Beta→Main hub AgentTool calls.

`--scenario relay` (default) replays the authoritative relay campaign unchanged.
`--scenario pressure` runs the retention-pressure Waiting scenario: Main's
unbounded hub jobs-wait must fail closed with the typed pruned-missing error
after real filler pressure prunes a settled sibling mid-wait.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import secrets
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
PRESSURE_A_SENTINEL = "PRESSURE_CHILD_A_OWNED_HUB_SENTINEL"
PRESSURE_B_SENTINEL = "PRESSURE_CHILD_B_OWNED_HUB_SENTINEL"
PRESSURE_PRODUCER_SENTINEL = "PRESSURE_PRODUCER_OWNED_HUB_SENTINEL"
PRESSURE_FILLER_SENTINEL = "PRESSURE_FILLER_OWNED_HUB_SENTINEL"
A_TO_PRODUCER = "pressure-a-to-producer-trigger"
PRODUCER_TO_B = "pressure-producer-to-b-release"
FILLER_CONTEXT = "Retention-pressure filler: settle immediately so job retention pressure accumulates."
FILLER_BRIEF = "Settle immediately: end your turn right away with the single word done. Acceptance: your final message is exactly done."
PRESSURE_WAIT_RUNNING_CARD = "Hub wait · running"
PRESSURE_WAIT_FAILED_CARD = "Hub wait · failed"
PRESSURE_MISSING_MARKER = "pruned or removed while waited"
PRESSURE_TIMEOUT_MARKER = "No job completed before timeout."
PRESSURE_BATCH_SIZE = 64
PRESSURE_MAX_BATCHES = 5
PRESSURE_UI_SKIP_MARKER = "UI skipped"
PRESSURE_NAV_PAGE_ROWS = 36


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

def message_text(message: dict[str, Any]) -> str:
    content = message.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(
            str(block.get("text", ""))
            for block in content
            if isinstance(block, dict) and block.get("type") == "text"
        )
    return ""


def tool_result_text(body: dict[str, Any], call_id: str) -> str | None:
    for message in body.get("messages", []):
        if message.get("role") != "tool" or message.get("tool_call_id") != call_id:
            continue
        return message_text(message)
    return None


def latest_tool_result(body: dict[str, Any]) -> tuple[str, str] | None:
    for message in reversed(body.get("messages", [])):
        if message.get("role") == "tool":
            return str(message.get("tool_call_id", "")), message_text(message)
    return None


def route_of(serialized: str) -> str:
    if PRESSURE_FILLER_SENTINEL in serialized:
        return "filler"
    if PRESSURE_A_SENTINEL in serialized:
        return "a"
    if PRESSURE_B_SENTINEL in serialized:
        return "b"
    if PRESSURE_PRODUCER_SENTINEL in serialized:
        return "producer"
    if ALPHA_SENTINEL in serialized:
        return "alpha"
    if BETA_SENTINEL in serialized:
        return "beta"
    return "main"


_SPAWN_LINE = re.compile(r"\] (\S+) \(\S+\) — queued as job (\S+)")


def task_spawn_job_ids(body: dict[str, Any], call_id: str, names: tuple[str, ...]) -> dict[str, str]:
    """Map child names to the real job ids reported by the task tool result."""
    text = tool_result_text(body, call_id) or ""
    found = {agent_id: job_id for agent_id, job_id in _SPAWN_LINE.findall(text)}
    return {name: found[name] for name in names if name in found}


class HubTuiHandler(BaseHTTPRequestHandler):
    lock = threading.Lock()
    states = {"main": 0, "alpha": 0, "beta": 0}
    transitions: list[str] = []
    request_error: BaseException | None = None
    requests: list[dict[str, Any]] = []
    completed = threading.Event()
    alpha_wait_issued = threading.Event()
    beta_wait_issued = threading.Event()
    scenario = "relay"
    pressure_states = {"main": 0, "a": 0, "b": 0, "producer": 0, "filler": 0}
    main_wait_running = threading.Event()
    pressure_job_ids: dict[str, str] = {}
    pressure_wait_error: str | None = None
    producer_batches_spawned = 0
    producer_batch_ids: list[str] = []

    def do_POST(self) -> None:
        try:
            length = int(self.headers.get("content-length", "0"))
            body = json.loads(self.rfile.read(length))
            serialized = json.dumps(body, separators=(",", ":"))
            cls = type(self)
            route = route_of(serialized)
            cls.requests.append({"route": route, "body": body})
            if cls.scenario != "pressure":
                names = tool_names(body)
                if route == "main" and not {"task", "hub"}.issubset(names):
                    raise AssertionError(f"Main request omitted real task/hub tools: {sorted(names)!r}")
                if route != "main" and "hub" not in names:
                    raise AssertionError(f"{route} request omitted child-owned hub tool: {sorted(names)!r}")
            states = cls.pressure_states if cls.scenario == "pressure" else cls.states
            with cls.lock:
                state = states[route]
                states[route] = state + 1
                cls.transitions.append(f"{route}:{state}")
            if cls.scenario == "pressure":
                response = cls.pressure_response(route, state, body)
            else:
                response = cls.relay_response(route, state, body)
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

    @classmethod
    def relay_response(cls, route: str, state: int, body: dict[str, Any]) -> bytes:
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
            if not cls.alpha_wait_issued.wait(5) or not cls.beta_wait_issued.wait(5):
                raise AssertionError("children did not issue their hub waits before Main send")
            response = tool_response(route, "main-send-alpha", "hub", {"op": "send", "to": "Alpha", "message": MAIN_TO_ALPHA})
        elif route == "main" and state == 2:
            send_result = tool_result_text(body, "main-send-alpha")
            if send_result != "- Alpha: processed":
                raise AssertionError(f"Main did not receive its exact hub send result: {send_result!r}")
            response = tool_response(route, "main-wait-beta", "hub", {"op": "wait", "from": "Beta", "timeoutMs": 10000})
        elif route == "main" and state == 3:
            wait_result = tool_result_text(body, "main-wait-beta")
            if wait_result is None or not wait_result.endswith(f"Beta: {BETA_TO_MAIN}"):
                raise AssertionError(f"Main hub wait did not receive Beta's exact body: {wait_result!r}")
            response = tool_response(route, "main-jobs", "hub", {"op": "jobs"})
        elif route == "main" and state == 4:
            if not has_tool_result(body, "main-jobs"):
                raise AssertionError("Main did not receive its hub jobs result")
            response = tool_response(route, "main-list", "hub", {"op": "list"})
        elif route == "main" and state == 5:
            if not has_tool_result(body, "main-list"):
                raise AssertionError("Main did not receive its hub list result")
            response = tool_response(route, "main-timeout", "hub", {"op": "wait", "from": "Beta", "timeoutMs": 10})
        elif route == "main" and state == 6:
            timeout_result = tool_result_text(body, "main-timeout")
            if timeout_result != "No message before timeout.":
                raise AssertionError(f"Main hub wait did not produce the exact neutral timeout: {timeout_result!r}")
            response = text_response(route, "hub tool campaign complete")
            cls.completed.set()
        elif route == "alpha" and state == 0:
            cls.alpha_wait_issued.set()
            response = tool_response(route, "alpha-wait-main", "hub", {"op": "wait", "from": "Main", "timeoutMs": 10000})
        elif route == "alpha" and state == 1:
            wait_result = tool_result_text(body, "alpha-wait-main")
            if wait_result is None or not wait_result.endswith(f"Main: {MAIN_TO_ALPHA}"):
                raise AssertionError(f"Alpha hub wait did not receive Main's exact body: {wait_result!r}")
            if not cls.beta_wait_issued.wait(5):
                raise AssertionError("Beta did not issue its hub wait before Alpha send")
            response = tool_response(route, "alpha-send-beta", "hub", {"op": "send", "to": "Beta", "message": ALPHA_TO_BETA})
        elif route == "alpha" and state == 2:
            if not has_tool_result(body, "alpha-send-beta"):
                raise AssertionError("Alpha did not receive its owned hub send result")
            response = text_response(route, "Alpha hub relay complete")
        elif route == "beta" and state == 0:
            cls.beta_wait_issued.set()
            response = tool_response(route, "beta-wait-alpha", "hub", {"op": "wait", "from": "Alpha", "timeoutMs": 10000})
        elif route == "beta" and state == 1:
            wait_result = tool_result_text(body, "beta-wait-alpha")
            if wait_result is None or not wait_result.endswith(f"Alpha: {ALPHA_TO_BETA}"):
                raise AssertionError(f"Beta hub wait did not receive Alpha's exact body: {wait_result!r}")
            response = tool_response(route, "beta-send-main", "hub", {"op": "send", "to": "Main", "message": BETA_TO_MAIN})
        elif route == "beta" and state == 2:
            if not has_tool_result(body, "beta-send-main"):
                raise AssertionError("Beta did not receive its owned hub send result")
            response = text_response(route, "Beta hub relay complete")
        else:
            raise AssertionError(f"unexpected provider request route={route} state={state}")
        return response

    @classmethod
    def pressure_response(cls, route: str, state: int, body: dict[str, Any]) -> bytes:
        if route == "filler":
            return text_response(route, "Filler settled immediately.")
        if route == "main":
            return cls.pressure_main(state, body)
        if route == "a":
            return cls.pressure_a(state, body)
        if route == "b":
            return cls.pressure_b(state, body)
        if route == "producer":
            return cls.pressure_producer(state, body)
        raise AssertionError(f"unexpected pressure route {route}")

    @classmethod
    def pressure_main(cls, state: int, body: dict[str, Any]) -> bytes:
        if state == 0:
            return tool_response(
                "main",
                "main-spawn-pressure",
                "task",
                {
                    "context": "Retention-pressure contract: A settles right after waking Producer; Producer drives filler batches through the real task/hub calls supplied by the localhost provider and releases B only once the real hub jobs listing no longer reports job A.",
                    "tasks": [
                        {"name": "A", "agent": "A", "task": "Send Producer its trigger through the hub call supplied by the localhost provider, then finish. Acceptance: Producer confirms the trigger body verbatim."},
                        {"name": "B", "agent": "B", "task": "Wait for Producer's release through the hub call supplied by the localhost provider, then finish. Acceptance: Producer's release body arrives verbatim."},
                        {"name": "Producer", "agent": "Producer", "task": "Wait for A's trigger, then run filler batches through the task and hub calls supplied by the localhost provider and release B as soon as the hub jobs listing stops reporting job A. Acceptance: B confirms the release."},
                    ],
                },
            )
        if state == 1:
            if not has_tool_result(body, "main-spawn-pressure"):
                raise AssertionError("Main did not receive the real task tool result")
            ids = task_spawn_job_ids(body, "main-spawn-pressure", ("A", "B"))
            missing = [name for name in ("A", "B") if name not in ids]
            if missing:
                raise AssertionError(f"Main task result did not report real job ids for {missing!r}")
            cls.pressure_job_ids = ids
            return tool_response(
                "main",
                "main-wait-jobs",
                "hub",
                {"op": "wait", "ids": [ids["A"], ids["B"]]},
            )
        if state == 2:
            result = tool_result_text(body, "main-wait-jobs")
            if result is None:
                raise AssertionError("Main did not receive its unbounded hub jobs-wait result")
            if PRESSURE_TIMEOUT_MARKER in result or "timed out" in result:
                raise AssertionError(f"Main jobs-wait ended as a timeout instead of the typed missing error: {result!r}")
            if PRESSURE_MISSING_MARKER not in result:
                raise AssertionError(f"Main jobs-wait did not fail closed with the typed missing error: {result!r}")
            if cls.pressure_job_ids["A"] not in result:
                raise AssertionError(f"typed missing error does not name the pruned job: {result!r}")
            if cls.pressure_job_ids["B"] in result:
                raise AssertionError(f"settled job B must not be reported missing: {result!r}")
            cls.pressure_wait_error = result
            cls.completed.set()
            return text_response("main", "pressure campaign complete")
        raise AssertionError(f"unexpected main request state {state}")

    @classmethod
    def pressure_a(cls, state: int, body: dict[str, Any]) -> bytes:
        if state == 0:
            # Runtime-backed handshake: block WITHOUT holding the handler lock
            # until the harness observed Main's running hub-wait card in the
            # real pane. The 120s bound is failure protection, never the
            # success path.
            if not cls.main_wait_running.wait(120):
                raise AssertionError(
                    "handshake failed: Main's running hub-wait card was never observed; "
                    "refusing to release the filler pressure on timing guesses"
                )
            return tool_response("a", "a-trigger-producer", "hub", {"op": "send", "to": "Producer", "message": A_TO_PRODUCER})
        if state == 1:
            result = tool_result_text(body, "a-trigger-producer")
            if result != "- Producer: processed":
                raise AssertionError(f"A's hub send did not wake Producer: {result!r}")
            return text_response("a", "A trigger role complete")
        raise AssertionError(f"unexpected a request state {state}")

    @classmethod
    def pressure_b(cls, state: int, body: dict[str, Any]) -> bytes:
        if state == 0:
            return tool_response("b", "b-wait-producer", "hub", {"op": "wait", "from": "Producer", "timeoutMs": 120000})
        if state == 1:
            result = tool_result_text(body, "b-wait-producer")
            if result is None or not result.endswith(f"Producer: {PRODUCER_TO_B}"):
                raise AssertionError(f"B hub wait did not receive Producer's exact release body: {result!r}")
            return text_response("b", "B relay role complete")
        raise AssertionError(f"unexpected b request state {state}")

    @classmethod
    def pressure_producer(cls, state: int, body: dict[str, Any]) -> bytes:
        if state == 0:
            return tool_response("producer", "producer-wait-a", "hub", {"op": "wait", "from": "A", "timeoutMs": 120000})
        latest = latest_tool_result(body)
        if latest is None:
            raise AssertionError("Producer request carried no tool result to advance the batch cycle")
        call_id, result = latest
        if call_id == "producer-wait-a":
            if result is None or not result.endswith(f"A: {A_TO_PRODUCER}"):
                raise AssertionError(f"Producer was not woken by A's real hub message: {result!r}")
            return cls.pressure_spawn_batch()
        if call_id.startswith("producer-spawn-"):
            batch = int(call_id.rsplit("-", 1)[-1])
            ids = re.findall(r"queued as job (\S+)", result)
            if len(ids) != PRESSURE_BATCH_SIZE:
                raise AssertionError(f"filler batch {batch} spawned {len(ids)} jobs, expected {PRESSURE_BATCH_SIZE}: {result!r}")
            cls.producer_batch_ids = ids
            return tool_response("producer", f"producer-wait-batch-{batch}", "hub", {"op": "wait", "ids": ids, "timeoutMs": 120000})
        if call_id.startswith("producer-wait-batch-"):
            batch = int(call_id.rsplit("-", 1)[-1])
            if PRESSURE_MISSING_MARKER in result or PRESSURE_TIMEOUT_MARKER in result:
                raise AssertionError(f"Producer's own batch-{batch} jobs-wait ended unexpectedly under retention pressure: {result!r}")
            return tool_response("producer", f"producer-jobs-{batch}", "hub", {"op": "jobs"})
        if call_id.startswith("producer-jobs-"):
            batch = int(call_id.rsplit("-", 1)[-1])
            target = cls.pressure_job_ids.get("A")
            if target is None:
                raise AssertionError("Producer cannot evaluate the jobs listing: job A's real id was never captured")
            if target not in result:
                return tool_response("producer", "producer-release-b", "hub", {"op": "send", "to": "B", "message": PRODUCER_TO_B})
            if batch >= PRESSURE_MAX_BATCHES:
                raise AssertionError(
                    f"job A survived {PRESSURE_MAX_BATCHES} filler batches "
                    f"({PRESSURE_MAX_BATCHES * PRESSURE_BATCH_SIZE} fillers); retention pressure did not materialize: {result!r}"
                )
            return cls.pressure_spawn_batch()
        if call_id == "producer-release-b":
            if result != "- B: processed":
                raise AssertionError(f"Producer's release hub send did not wake B: {result!r}")
            return text_response("producer", "Producer pressure role complete")
        raise AssertionError(f"unexpected producer tool result {call_id!r}: {result!r}")

    @classmethod
    def pressure_spawn_batch(cls) -> bytes:
        cls.producer_batches_spawned += 1
        batch = cls.producer_batches_spawned
        items = [
            {"name": f"Filler-{batch:02d}-{index:02d}", "agent": "Filler", "task": FILLER_BRIEF}
            for index in range(1, PRESSURE_BATCH_SIZE + 1)
        ]
        return tool_response("producer", f"producer-spawn-{batch}", "task", {"context": FILLER_CONTEXT, "tasks": items})


_TMUX_SOCKET = f"rpi-e2e-hub-{os.getpid()}-{secrets.token_hex(4)}"

def run_tmux(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.pop("TMUX", None)
    env.pop("TMUX_PANE", None)
    return subprocess.run(
        ["tmux", "-L", _TMUX_SOCKET, "-f", "/dev/null", *args],
        text=True,
        capture_output=True,
        check=check,
        env=env,
    )

def start_detached_session(session: str, workspace: str, command: list[str]) -> str:
    pane = f"{session}:0.0"
    run_tmux("new-session", "-d", "-s", session, "-x", "120", "-y", "40", "-c", workspace, "sleep", "86400")
    run_tmux("set-option", "-w", "-t", f"{session}:0", "remain-on-exit", "on")
    run_tmux("respawn-pane", "-k", "-t", pane, "-c", workspace, *command)
    return pane



def capture(session: str) -> str:
    return run_tmux("capture-pane", "-p", "-S", "-2000", "-t", f"{session}:0.0").stdout

def wait_for_session_exit(session: str, timeout: float) -> int:
    deadline = time.monotonic() + timeout
    dead_without_status = False
    while time.monotonic() < deadline:
        status = run_tmux(
            "display-message",
            "-p",
            "-t",
            f"{session}:0.0",
            "#{pane_dead} #{pane_dead_status}",
            check=False,
        )
        if status.returncode != 0:
            raise AssertionError("Hub TUI session vanished before its exit status was captured")
        dead, _, exit_status = status.stdout.strip().partition(" ")
        if dead != "1":
            time.sleep(0.1)
            continue
        exit_status = exit_status.strip()
        if not exit_status:
            dead_without_status = True
            time.sleep(0.1)
            continue
        try:
            exit_code = int(exit_status)
        except ValueError:
            raise AssertionError(
                f"tmux reported a non-numeric pane exit status {exit_status!r}"
            ) from None
        if exit_code < 0:
            raise AssertionError(
                f"Hub TUI process terminated by signal (pane_dead_status={exit_code}); "
                "application cleanup must exit cleanly"
            )
        return exit_code
    if dead_without_status:
        raise AssertionError(
            "tmux reported pane_dead=1 but never published a numeric pane_dead_status "
            "before the deadline"
        )
    raise AssertionError("Application cleanup did not terminate the Hub TUI process")


def normalized(text: str) -> str:
    """Collapse whitespace runs so status needles match normal pane spacing."""
    return " ".join(text.split())


def wait_for(session: str, needles: list[str], timeout: float, forbidden: list[str] | None = None) -> str:
    deadline = time.monotonic() + timeout
    latest = ""
    while time.monotonic() < deadline:
        latest = capture(session)
        flat = normalized(latest)
        for marker in forbidden or []:
            if normalized(marker) in flat:
                raise AssertionError(f"TUI displayed forbidden {marker!r}; final pane:\n{latest}")
        if all(normalized(needle) in flat for needle in needles):
            return latest
        if HubTuiHandler.request_error is not None:
            raise HubTuiHandler.request_error
        time.sleep(0.1)
    raise AssertionError(f"TUI did not display {needles!r}; final pane:\n{latest}")


def pressure_provider_state(batches: int) -> dict[str, int]:
    """Terminal request counts for a real pressure run with `batches` filler batches."""
    return {
        "main": 3,
        "a": 2,
        "b": 2,
        "producer": 3 * batches + 3,
        "filler": PRESSURE_BATCH_SIZE * batches,
    }


def wait_for_pressure_provider(deadline: float) -> int:
    """Wait for the real provider's terminal counts, tracking the actual batch count."""
    while time.monotonic() < deadline:
        if HubTuiHandler.request_error is not None:
            raise HubTuiHandler.request_error
        batches = HubTuiHandler.producer_batches_spawned
        if (
            batches >= 1
            and HubTuiHandler.completed.is_set()
            and HubTuiHandler.pressure_states == pressure_provider_state(batches)
        ):
            return batches
        time.sleep(0.1)
    if HubTuiHandler.request_error is not None:
        raise HubTuiHandler.request_error
    raise AssertionError(
        "pressure provider did not reach its terminal request counts: "
        f"{HubTuiHandler.pressure_states!r} with {HubTuiHandler.producer_batches_spawned} batches"
    )


def wait_for_frame_change(
    session: str, previous: str, deadline: float, needles: tuple[str, ...] = ()
) -> str:
    """Return the next real capture once navigation repaints or the needles render."""
    wanted = [normalized(needle) for needle in needles]
    while time.monotonic() < deadline:
        if HubTuiHandler.request_error is not None:
            raise HubTuiHandler.request_error
        current = capture(session)
        flat = normalized(current)
        if current != previous or (wanted and all(needle in flat for needle in wanted)):
            return current
        time.sleep(0.1)
    raise AssertionError("pressure navigation did not repaint the terminal before the deadline")


def assert_pressure_settled(pages: list[str], batches: int) -> None:
    """Prove every retained aggregate-card row is settled and no child is queued/running."""
    union = normalized("\n".join(pages))
    retained = 3 + PRESSURE_BATCH_SIZE * batches
    if f"Task {retained} agents" not in union:
        raise AssertionError(f"retained pressure card omitted its {retained}-agent header")
    expected = [("A", "A"), ("B", "B"), ("Producer", "Producer")]
    expected.extend(
        (f"Filler-{batch:02d}-{index:02d}", "Filler")
        for batch in range(1, batches + 1)
        for index in range(1, PRESSURE_BATCH_SIZE + 1)
    )
    for name, role in expected:
        settled = re.compile(rf"• {re.escape(name)} ⟦{re.escape(role)}⟧ · completed(?: ·|$)")
        if not settled.search(union):
            raise AssertionError(f"retained pressure card omitted settled child {name!r}")
    if re.search(r"• .+⟦.+⟧ · (queued|running)", union):
        raise AssertionError("retained pressure card still reports queued or running children")


def assert_transcript(pane: str) -> None:
    lines = pane.splitlines()
    for expected in ["✓ Hub jobs · complete", "✓ Hub agents · complete", "• Hub wait · timed out · no message arrived", "✉ IRC ⟵ Beta received", BETA_TO_MAIN]:
        if not any(normalized(expected) in normalized(line) for line in lines):
            raise AssertionError(f"transcript omitted {expected!r}:\n{pane}")

    for title in ["✓ Hub jobs · complete", "✓ Hub agents · complete", "• Hub wait · timed out · no message arrived"]:
        matches = [line for line in lines if normalized(title) in normalized(line)]
        if any(marker in line for line in matches for marker in "╭╰│"):
            raise AssertionError(f"metadata-only Hub operation retained an empty frame for {title!r}: {matches!r}")

    title_index = next(index for index, line in enumerate(lines) if normalized("✉ IRC ⟵ Beta received") in normalized(line))
    if "╭──" not in lines[title_index] or title_index + 2 >= len(lines):
        raise AssertionError(f"typed IRC title lost its frame:\n{pane}")
    if BETA_TO_MAIN not in lines[title_index + 1] or "│" not in lines[title_index + 1]:
        raise AssertionError(f"typed IRC body lost its framed row:\n{pane}")
    if "╰" not in lines[title_index + 2]:
        raise AssertionError(f"typed IRC card lost its bottom frame:\n{pane}")
    if "<orchestration-message" in pane:
        raise AssertionError("raw orchestration XML leaked into public TUI")
    if "orchestration shut down" in pane.lower():
        raise AssertionError(f"orchestration stopped before child jobs completed:\n{pane}")


def assert_pressure_transcript(pane: str) -> None:
    flat = normalized(pane)
    for expected in ["pressure campaign complete", PRESSURE_WAIT_FAILED_CARD]:
        if normalized(expected) not in flat:
            raise AssertionError(f"pressure transcript omitted {expected!r}:\n{pane}")
    for forbidden in [PRESSURE_WAIT_RUNNING_CARD, "Wait timed out", PRESSURE_TIMEOUT_MARKER, PRESSURE_UI_SKIP_MARKER]:
        if normalized(forbidden) in flat:
            raise AssertionError(f"pressure transcript retained {forbidden!r}:\n{pane}")
    if "<orchestration-message" in pane:
        raise AssertionError("raw orchestration XML leaked into public TUI")
    if "orchestration shut down" in pane.lower():
        raise AssertionError(f"orchestration stopped before the pressure campaign completed:\n{pane}")


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
    parser.add_argument("--scenario", choices=("relay", "pressure"), default="relay")
    args = parser.parse_args()
    pressure = args.scenario == "pressure"
    HubTuiHandler.scenario = args.scenario

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
        json.dumps(
            {
                "orchestration": {
                    "tasks": True,
                    "todo": False,
                    "maxConcurrency": 8 if pressure else 2,
                    "maxRecursionDepth": 3 if pressure else 2,
                }
            }
        ),
        encoding="utf-8",
    )
    if pressure:
        write_agent(agents_dir / "a.md", "A", PRESSURE_A_SENTINEL)
        write_agent(agents_dir / "b.md", "B", PRESSURE_B_SENTINEL)
        write_agent(agents_dir / "producer.md", "Producer", PRESSURE_PRODUCER_SENTINEL)
        write_agent(agents_dir / "filler.md", "Filler", PRESSURE_FILLER_SENTINEL)
    else:
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
        "RPI_SKIP_VERSION_CHECK=1",
        "TERM=xterm-256color",
    ]
    try:
        pane = start_detached_session(
            session,
            str(workspace),
            ["env", *environment, rpi, "--model", "hub-tui-e2e/mock", "--api-key", "localhost-hub-e2e-not-a-secret"],
        )
        time.sleep(1.5)
        run_tmux("send-keys", "-t", pane, "-l", "Run the authoritative child-owned hub campaign.")
        run_tmux("send-keys", "-t", pane, "Enter")
        if pressure:
            observed = wait_for(session, [PRESSURE_WAIT_RUNNING_CARD], 60, forbidden=[PRESSURE_UI_SKIP_MARKER])
            (evidence / "pressure-wait-running.txt").write_text(observed, encoding="utf-8")
            HubTuiHandler.main_wait_running.set()
            deadline = time.monotonic() + 240
            batches = wait_for_pressure_provider(deadline)
            pages = [capture(session)]
            (evidence / "navigation-00.txt").write_text(pages[0], encoding="utf-8")
            needles = ["pressure campaign complete", PRESSURE_WAIT_FAILED_CARD]
            max_pages = 2 + (
                3 + PRESSURE_BATCH_SIZE * batches + PRESSURE_NAV_PAGE_ROWS - 1
            ) // PRESSURE_NAV_PAGE_ROWS
            for index in range(1, max_pages):
                union = normalized("\n".join(pages))
                if all(normalized(needle) in union for needle in needles):
                    break
                run_tmux("send-keys", "-t", pane, "PageUp")
                pages.append(wait_for_frame_change(session, pages[-1], deadline, tuple(needles)))
                (evidence / f"navigation-{index:02d}.txt").write_text(pages[-1], encoding="utf-8")
            transcript = "\n".join(pages)
            assert_pressure_transcript(transcript)
            assert_pressure_settled(pages, batches)
            # PageDown restores transcript follow; End moves the composer cursor.
            final = pages[-1]
            tail = f"Filler-{batches:02d}-{PRESSURE_BATCH_SIZE:02d} ⟦Filler⟧ · completed"
            for _ in range(len(pages) - 1):
                if normalized(tail) in normalized(final):
                    break
                run_tmux("send-keys", "-t", pane, "PageDown")
                final = wait_for_frame_change(session, final, deadline)
            if normalized(tail) not in normalized(final):
                raise AssertionError(f"PageDown did not restore the settled follow tail {tail!r}:\n{final}")
            (evidence / "transcript.txt").write_text(transcript, encoding="utf-8")
            (evidence / "navigation-final.txt").write_text(final, encoding="utf-8")
        else:
            transcript = wait_for(
                session,
                [
                    "hub tool campaign complete",
                    "IRC ⟵ Beta",
                    BETA_TO_MAIN,
                    "✓ Hub jobs · complete",
                    "✓ Hub agents · complete",
                    "Hub wait · timed out · no message arrived",
                ],
                35,
            )
            assert_transcript(transcript)
            (evidence / "transcript.txt").write_text(transcript, encoding="utf-8")
            run_tmux("send-keys", "-t", pane, "-l", "/jobs")
            run_tmux("send-keys", "-t", pane, "Enter")
            jobs_overlay = wait_for(session, ["2 completed", "completed Alpha", "completed Beta"], 10)
            if HubTuiHandler.request_error is not None:
                raise HubTuiHandler.request_error
            expected_states = {"main": 7, "alpha": 3, "beta": 3}
            if HubTuiHandler.states != expected_states or not HubTuiHandler.completed.is_set():
                raise AssertionError(f"incomplete provider route states: {HubTuiHandler.states!r}")
            if "<orchestration-message" in jobs_overlay:
                raise AssertionError("raw orchestration XML leaked into jobs overlay")
            if "orchestration shut down" in jobs_overlay.lower():
                raise AssertionError(f"orchestration stopped before completed jobs were observable:\n{jobs_overlay}")
            (evidence / "jobs-overlay.txt").write_text(jobs_overlay, encoding="utf-8")
        if pressure:
            if HubTuiHandler.request_error is not None:
                raise HubTuiHandler.request_error
            if HubTuiHandler.pressure_wait_error is None:
                raise AssertionError("pressure scenario finished without the typed missing-error evidence")
            (evidence / "pressure-wait-error.txt").write_text(HubTuiHandler.pressure_wait_error, encoding="utf-8")
            batches = HubTuiHandler.producer_batches_spawned
            if batches < 1:
                raise AssertionError("pressure scenario released no filler batches")
            expected_states = {
                "main": 3,
                "a": 2,
                "b": 2,
                "producer": 3 * batches + 3,
                "filler": PRESSURE_BATCH_SIZE * batches,
            }
            if HubTuiHandler.pressure_states != expected_states:
                raise AssertionError(f"incomplete pressure provider route states: {HubTuiHandler.pressure_states!r} != {expected_states!r}")
        run_tmux("send-keys", "-t", pane, "Escape")
        run_tmux("send-keys", "-t", pane, "-l", "/quit")
        run_tmux("send-keys", "-t", pane, "Enter")
        exit_status = wait_for_session_exit(session, 60 if pressure else 10)
        if exit_status != 0:
            raise AssertionError(f"Application cleanup exited with status {exit_status}")
        (evidence / "provider-transitions.json").write_text(
            json.dumps(
                {
                    "scenario": args.scenario,
                    "states": HubTuiHandler.states,
                    "pressure_states": HubTuiHandler.pressure_states,
                    "pressure_job_ids": HubTuiHandler.pressure_job_ids,
                    "producer_batches_spawned": HubTuiHandler.producer_batches_spawned,
                    "transitions": HubTuiHandler.transitions,
                },
                indent=2,
            ),
            encoding="utf-8",
        )
        if pressure:
            checks = [
                "real-task-tool-spawn",
                "main-unbounded-jobs-wait-parked-before-pressure",
                "a-real-hub-send-released-producer-after-observed-running-wait",
                "producer-real-batch-spawns-from-real-job-ids",
                "producer-jobs-confirmed-target-pruned-before-releasing-b",
                "b-real-hub-wait-received-release",
                "main-typed-missing-error-names-exact-pruned-job-id",
                "non-timeout-resolution",
                "ui-failed-wait-card-no-running-wait-remaining",
                "bounded-pageup-union-shows-final-reply-and-failed-wait",
                "retained-pressure-card-settled-no-queued-running",
                "pagedown-restored-settled-follow-tail",
                "final-turn-terminal",
                "application-cleanup-exit",
            ]
        else:
            checks = [
                "real-task-tool-spawn",
                "main-parent-hub-send",
                "alpha-child-owned-hub-wait-send",
                "beta-child-owned-hub-wait-send",
                "visible-beta-to-main-irc",
                "typed-irc-frame-shape",
                "compact-hub-jobs-operation",
                "compact-hub-agents-operation",
                "application-cleanup-exit",
                "neutral-hub-wait-timeout",
                "separate-transcript-jobs-evidence",
                "alpha-beta-completed-jobs",
                "no-premature-orchestration-shutdown",
            ]
        (evidence / "assertions.json").write_text(
            json.dumps({"status": "passed", "scenario": args.scenario, "checks": checks}, indent=2),
            encoding="utf-8",
        )
    except BaseException as error:
        (evidence / "failure-tui.txt").write_text(capture(session), encoding="utf-8")
        (evidence / "failure-provider.json").write_text(
            json.dumps(
                {
                    "scenario": HubTuiHandler.scenario,
                    "request_error": repr(error),
                    "states": HubTuiHandler.states,
                    "pressure_states": HubTuiHandler.pressure_states,
                    "pressure_job_ids": HubTuiHandler.pressure_job_ids,
                    "pressure_wait_error": HubTuiHandler.pressure_wait_error,
                    "producer_batches_spawned": HubTuiHandler.producer_batches_spawned,
                    "producer_batch_ids": HubTuiHandler.producer_batch_ids,
                    "transitions": HubTuiHandler.transitions,
                    "requests": HubTuiHandler.requests,
                },
                indent=2,
            ),
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
