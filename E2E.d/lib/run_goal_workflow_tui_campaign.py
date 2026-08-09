#!/usr/bin/env python3
"""Drive exact Chinese goal/workflow commands through a real rpi TUI and localhost model."""

from __future__ import annotations

import argparse
import json
import os
import re
import secrets
import subprocess
import threading
import time
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

ZIG_WORKFLOW_OBJECTIVE = "调研并且撰写zig版本的pi-coding-agent"
GOAL_OBJECTIVE = "制作zig版本的pi-coding-agent"
MOONBIT_WORKFLOW_OBJECTIVE = "制作moonbit版本的pi-coding-agent"
COMMANDS = [
    f"/workflow create {ZIG_WORKFLOW_OBJECTIVE}",
    f"/workflow create zig-agent {ZIG_WORKFLOW_OBJECTIVE}",
    f"/goal {GOAL_OBJECTIVE}",
    f"/workflow create moonbit-agent {MOONBIT_WORKFLOW_OBJECTIVE}",
    f"/workflow create {MOONBIT_WORKFLOW_OBJECTIVE}",
]


def stream_response(payload: dict[str, Any]) -> bytes:
    return (
        f"data: {json.dumps(payload, separators=(',', ':'), ensure_ascii=False)}\n\n"
        "data: [DONE]\n\n"
    ).encode()


def assistant_text(response_id: str, text: str) -> bytes:
    return stream_response(
        {
            "id": response_id,
            "model": "goal-workflow-mock",
            "choices": [
                {
                    "index": 0,
                    "delta": {"content": text},
                    "finish_reason": "stop",
                }
            ],
            "usage": {"prompt_tokens": 8, "completion_tokens": 4, "total_tokens": 12},
        }
    )


def tool_call(response_id: str, call_id: str, name: str, arguments: dict[str, Any]) -> bytes:
    return stream_response(
        {
            "id": response_id,
            "model": "goal-workflow-mock",
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
                                    "arguments": json.dumps(
                                        arguments, separators=(",", ":"), ensure_ascii=False
                                    ),
                                },
                            }
                        ]
                    },
                    "finish_reason": "tool_calls",
                }
            ],
            "usage": {"prompt_tokens": 8, "completion_tokens": 4, "total_tokens": 12},
        }
    )


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


def delegated_assignment(text: str) -> str | None:
    match = re.search(r"<delegated_assignment>\s*(.*?)\s*</delegated_assignment>", text, re.DOTALL)
    return match.group(1).strip() if match else None


class CampaignState:
    def __init__(self) -> None:
        self.condition = threading.Condition()
        self.serial = 0
        self.error: BaseException | None = None
        self.events: list[dict[str, Any]] = []
        self.requests: list[dict[str, Any]] = []
        self.last_unclassified: dict[str, Any] | None = None
        self.workflow_order: list[str] = []
        self.workflow_objectives: dict[str, str] = {}
        self.workflow_todo_results: dict[str, str] = {}
        self.workflow_assignments: dict[str, set[str]] = {}
        self.worker_completions: Counter[str] = Counter()
        self.goal_todo_result: str | None = None
        self.goal_assignments = {
            "complete active goal Zig research",
            "complete active goal Zig validation",
        }

    def next_id(self, prefix: str) -> str:
        with self.condition:
            self.serial += 1
            return f"{prefix}-{self.serial}"

    def record(self, event: dict[str, Any]) -> None:
        with self.condition:
            self.events.append(event)
            self.condition.notify_all()

    def fail(self, error: BaseException) -> None:
        with self.condition:
            if self.error is None:
                self.error = error
            self.condition.notify_all()

    def record_request(self, body: dict[str, Any]) -> None:
        text = message_text(body)
        entry = {
            "tools": sorted(tool_names(body)),
            "message": text[-4000:],
            "roles": [message.get("role") for message in body.get("messages") or []],
        }
        with self.condition:
            self.requests.append(entry)
            self.condition.notify_all()

    def record_unclassified(self, text: str, names: set[str]) -> None:
        entry = {"tools": sorted(names), "message": text[-4000:]}
        with self.condition:
            self.last_unclassified = entry
            self.events.append({"kind": "unclassified", **entry})
            self.condition.notify_all()

    def wait(self, description: str, predicate: Any, timeout: float = 30.0) -> None:
        deadline = time.monotonic() + timeout
        with self.condition:
            while not predicate():
                if self.error is not None:
                    raise self.error
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError(f"timed out waiting for {description}")
                self.condition.wait(min(remaining, 0.25))


STATE = CampaignState()


class GoalWorkflowHandler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:
        try:
            length = int(self.headers.get("content-length", "0"))
            body = json.loads(self.rfile.read(length))
            STATE.record_request(body)
            response = self.route(body)
        except BaseException as error:
            STATE.fail(error)
            response = assistant_text(STATE.next_id("error"), "mock request rejected")
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)

    def route(self, body: dict[str, Any]) -> bytes:
        text = message_text(body)
        names = tool_names(body)
        supervisor = re.search(
            r"You plan workflow\s+([^\s.]+)\.\s*Objective:\s*([^\n<]+)", text
        )
        if supervisor:
            return self.route_supervisor(body, text, names, supervisor)

        assignment = delegated_assignment(text)
        if assignment is not None:
            if "hub" not in names:
                raise AssertionError(f"worker request omitted hub tool: {sorted(names)!r}")
            STATE.record(
                {
                    "kind": "worker",
                    "assignment": assignment,
                    "tools": sorted(names),
                    "message": text[-2000:],
                }
            )
            with STATE.condition:
                STATE.worker_completions[assignment] += 1
                STATE.condition.notify_all()
            return assistant_text(
                STATE.next_id("worker"), f"worker completed: {assignment}"
            )

        if GOAL_OBJECTIVE in text and {"todo", "goal"}.issubset(names):
            return self.route_goal(body, text, names)

        STATE.record_unclassified(text, names)
        raise AssertionError(
            f"unclassified provider request; tools={sorted(names)!r}, message={text[-1000:]!r}"
        )

    def route_supervisor(
        self,
        body: dict[str, Any],
        text: str,
        names: set[str],
        supervisor: re.Match[str],
    ) -> bytes:
        workflow_id, objective = supervisor.groups()
        objective = objective.strip()
        if not {"todo", "task", "hub"}.issubset(names):
            raise AssertionError(
                f"workflow supervisor omitted todo/task/hub: {sorted(names)!r}"
            )
        todo_results = completed_tool_results(body, "todo")
        with STATE.condition:
            if workflow_id not in STATE.workflow_objectives:
                STATE.workflow_order.append(workflow_id)
                STATE.workflow_objectives[workflow_id] = objective
            STATE.condition.notify_all()
        if todo_results:
            result = todo_results[-1]
            if "Overall:" not in result:
                raise AssertionError(f"workflow todo result was not executable proof: {result!r}")
            with STATE.condition:
                STATE.workflow_todo_results[workflow_id] = result
                STATE.condition.notify_all()
            STATE.record(
                {
                    "kind": "workflow-todo-result",
                    "workflowId": workflow_id,
                    "objective": objective,
                    "result": result,
                }
            )
            return assistant_text(
                STATE.next_id("workflow-planned"),
                f"workflow {workflow_id} plan accepted",
            )

        language = "Moonbit" if "moonbit" in objective else "Zig"
        assignments = {
            f"worker research {language} workflow {workflow_id}",
            f"worker implement {language} workflow {workflow_id}",
        }
        with STATE.condition:
            STATE.workflow_assignments[workflow_id] = assignments
            STATE.condition.notify_all()
        phases = [
            (f"{language} Research", [f"worker research {language} workflow {workflow_id}"]),
            (
                f"{language} Implementation",
                [f"worker implement {language} workflow {workflow_id}"],
            ),
        ]
        STATE.record(
            {
                "kind": "workflow-todo-call",
                "workflowId": workflow_id,
                "objective": objective,
                "tools": sorted(names),
                "message": text[-2000:],
                "phases": phases,
            }
        )
        return tool_call(
            STATE.next_id("workflow-plan"),
            STATE.next_id("call-workflow-todo"),
            "todo",
            todo_init(phases),
        )

    def route_goal(
        self, body: dict[str, Any], text: str, names: set[str]
    ) -> bytes:
        todo_results = completed_tool_results(body, "todo")
        task_results = completed_tool_results(body, "task")
        if task_results:
            result = task_results[-1]
            if (
                "GoalResearch" not in result
                or "GoalValidate" not in result
                or "queued as job" not in result
            ):
                raise AssertionError(f"goal task result was not executable proof: {result!r}")
            return assistant_text(STATE.next_id("goal-planned"), "goal workers started")
        if todo_results:
            result = todo_results[-1]
            if "Overall:" not in result:
                raise AssertionError(f"goal todo result was not executable proof: {result!r}")
            with STATE.condition:
                STATE.goal_todo_result = result
                STATE.condition.notify_all()
            STATE.record({"kind": "goal-todo-result", "result": result})
            return tool_call(
                STATE.next_id("goal-workers"),
                STATE.next_id("call-goal-task"),
                "task",
                {
                    "name": None,
                    "agent": None,
                    "task": None,
                    "todoTaskId": None,
                    "context": "Shared goal contract: both workers operate on the same active goal and its Zig objective; complete the assigned phase and report executable proof.",
                    "tasks": [
                        {
                            "name": "GoalResearch",
                            "agent": "worker",
                            "todoTaskId": None,
                            "task": "complete active goal Zig research",
                        },
                        {
                            "name": "GoalValidate",
                            "agent": "worker",
                            "todoTaskId": None,
                            "task": "complete active goal Zig validation",
                        },
                    ],
                },
            )
        phases = [
            ("Goal Zig Research", ["analyze active goal Zig"]),
            ("Goal Zig Validation", ["validate active goal Zig"]),
        ]
        STATE.record(
            {
                "kind": "goal-todo-call",
                "objective": GOAL_OBJECTIVE,
                "tools": sorted(names),
                "message": text[-2000:],
                "phases": phases,
            }
        )
        return tool_call(
            STATE.next_id("goal-plan"),
            STATE.next_id("call-goal-todo"),
            "todo",
            todo_init(phases),
        )

    def log_message(self, _format: str, *_args: object) -> None:
        return


def run(*args: str, check: bool = True, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(list(args), text=True, capture_output=True, check=check, env=env)


def tmux(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return run("tmux", *args, check=check)


def capture(session: str, history: bool = False) -> str:
    args = ["capture-pane", "-p"]
    if history:
        args.extend(["-S", "-2000"])
    args.extend(["-t", f"{session}:0"])
    return tmux(*args).stdout


def wait_for_all(session: str, needles: list[str], timeout: float = 20.0) -> str:
    deadline = time.monotonic() + timeout
    latest = ""
    while time.monotonic() < deadline:
        latest = capture(session)
        if all(needle in latest for needle in needles):
            return latest
        if STATE.error is not None:
            raise STATE.error
        time.sleep(0.15)
    raise AssertionError(f"TUI omitted {needles!r}; final pane:\n{latest}")

def select_todo_overview_row(session: str, identity: str, max_moves: int = 64) -> str:
    needle = identity[:17]
    latest = ""
    for _ in range(max_moves):
        latest = capture(session)
        if any("›" in line and needle in line for line in latest.splitlines()):
            return latest
        tmux("send-keys", "-t", f"{session}:0", "Down")
        time.sleep(0.05)
    raise AssertionError(f"Todo DAG overview never selected {needle!r}; final pane:\n{latest}")


def send_command(session: str, command: str) -> None:
    buffer = f"goal-workflow-{os.getpid()}"
    tmux("set-buffer", "-b", buffer, command)
    tmux("paste-buffer", "-b", buffer, "-t", f"{session}:0")
    tmux("delete-buffer", "-b", buffer, check=False)
    tmux("send-keys", "-t", f"{session}:0", "Enter")


def prepare_workspace(workspace: Path) -> None:
    workspace.mkdir(parents=True, exist_ok=True)
    run("git", "init", "-q", str(workspace))
    run("git", "-C", str(workspace), "config", "user.email", "goal-workflow-e2e@example.com")
    run("git", "-C", str(workspace), "config", "user.name", "Goal Workflow E2E")
    run("git", "-C", str(workspace), "config", "commit.gpgsign", "false")
    (workspace / "campaign.txt").write_text("isolated goal workflow campaign\n", encoding="utf-8")
    run("git", "-C", str(workspace), "add", "--", "campaign.txt")
    run(
        "git",
        "-C",
        str(workspace),
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "-m",
        "seed isolated workflow campaign",
    )


def prepare_home(home: Path, port: int) -> Path:
    agent_dir = home / ".pi" / "agent"
    agents = agent_dir / "agents"
    agents.mkdir(parents=True, exist_ok=True)
    (agent_dir / "settings.json").write_text(
        json.dumps(
            {
                "orchestration": {
                    "tasks": True,
                    "todo": True,
                    "maxConcurrency": 4,
                    "maxRecursionDepth": 2,
                    "mailboxCapacity": 100,
                },
                "selector": {"autoSelectThreshold": 0},
            }
        ),
        encoding="utf-8",
    )
    (agent_dir / "models.json").write_text(
        json.dumps(
            {
                "providers": {
                    "goal-workflow-e2e": {
                        "baseUrl": f"http://127.0.0.1:{port}",
                        "api": "openai-completions",
                        "compat": {"supportsStrictMode": True},
                        "models": [
                            {
                                "id": "mock",
                                "name": "Goal Workflow Mock",
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
    (agents / "worker.md").write_text(
        """---
name: worker
description: Execute one exact workflow or goal Todo task
tools:
  - hub
---
Complete the delegated task and return a concise real worker result.
""",
        encoding="utf-8",
    )
    return agent_dir


def wait_workflow(index: int) -> str:
    STATE.wait(
        f"workflow {index + 1} todo result",
        lambda: len(STATE.workflow_order) > index
        and STATE.workflow_order[index] in STATE.workflow_todo_results,
    )
    with STATE.condition:
        workflow_id = STATE.workflow_order[index]
        expected = set(STATE.workflow_assignments[workflow_id])
    STATE.wait(
        f"workflow {workflow_id} worker completions",
        lambda: all(STATE.worker_completions[assignment] >= 1 for assignment in expected),
    )
    return workflow_id


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rpi", required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--evidence", required=True)
    args = parser.parse_args()

    rpi = str(Path(args.rpi).resolve())
    home = Path(args.home).resolve()
    workspace = Path(args.workspace).resolve()
    evidence = Path(args.evidence).resolve()
    home.mkdir(parents=True, exist_ok=True)
    evidence.mkdir(parents=True, exist_ok=True)
    prepare_workspace(workspace)

    server = ThreadingHTTPServer(("127.0.0.1", 0), GoalWorkflowHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    port = int(server.server_address[1])
    agent_dir = prepare_home(home, port)
    session = f"rpi-e2e-goal-workflow-{os.getpid()}"
    workflow_ids: list[str] = []
    try:
        tmux(
            "new-session",
            "-d",
            "-s",
            session,
            "-x",
            "140",
            "-y",
            "50",
            "-c",
            str(workspace),
            "env",
            f"HOME={home}",
            f"USERPROFILE={home}",
            f"PATH={os.environ.get('PATH', '')}",
            f"LANG={os.environ.get('LANG', 'C.UTF-8')}",
            f"LC_ALL={os.environ.get('LC_ALL', 'C.UTF-8')}",
            f"PI_CODING_AGENT_DIR={agent_dir}",
            "PI_SKIP_VERSION_CHECK=1",
            "TERM=xterm-256color",
            rpi,
            "--model",
            "goal-workflow-e2e/mock",
            "--api-key",
            secrets.token_urlsafe(24),
        )
        boot = wait_for_all(session, ["Welcome back!", "goal-workflow-e2e/mock"], timeout=30.0)
        (evidence / "boot.txt").write_text(boot, encoding="utf-8")
        if "exited" in boot.lower() or "RPI_EXIT=" in boot:
            raise AssertionError(f"rpi exited before campaign input:\n{boot}")

        send_command(session, COMMANDS[0])
        workflow_ids.append(wait_workflow(0))
        send_command(session, COMMANDS[1])
        workflow_ids.append(wait_workflow(1))

        send_command(session, COMMANDS[2])
        STATE.wait("goal todo tool result", lambda: STATE.goal_todo_result is not None)
        STATE.wait(
            "goal worker completions",
            lambda: all(
                STATE.worker_completions[assignment] >= 1
                for assignment in STATE.goal_assignments
            ),
        )

        send_command(session, COMMANDS[3])
        workflow_ids.append(wait_workflow(2))
        send_command(session, COMMANDS[4])
        workflow_ids.append(wait_workflow(3))
        if len(set(workflow_ids)) != 4:
            raise AssertionError(f"workflow IDs were not distinct: {workflow_ids!r}")
        with STATE.condition:
            objectives = [STATE.workflow_objectives[workflow_id] for workflow_id in workflow_ids]
        if objectives != [
            ZIG_WORKFLOW_OBJECTIVE,
            ZIG_WORKFLOW_OBJECTIVE,
            MOONBIT_WORKFLOW_OBJECTIVE,
            MOONBIT_WORKFLOW_OBJECTIVE,
        ]:
            raise AssertionError(f"workflow objective order changed: {objectives!r}")

        time.sleep(1.0)
        send_command(session, "/workflow")
        workflow_list = wait_for_all(
            session,
            [
                "Workflows · 4/4 · 0 active",
                "zig-agent",
                "moonbit-age",
                ZIG_WORKFLOW_OBJECTIVE[:5],
                MOONBIT_WORKFLOW_OBJECTIVE[:6],
            ],
        )
        (evidence / "workflow-list.txt").write_text(workflow_list, encoding="utf-8")
        tmux("send-keys", "-t", f"{session}:0", "Escape")
        time.sleep(0.3)

        send_command(session, "/todo")
        overview_needles = [
            "Todo DAGs",
            "Main session",
            "zig-agent",
            "moonbit-agent",
            ZIG_WORKFLOW_OBJECTIVE[:12],
            MOONBIT_WORKFLOW_OBJECTIVE[:12],
        ]
        overview = wait_for_all(session, overview_needles)
        if sum(workflow_id[:17] in overview for workflow_id in workflow_ids) != 4:
            raise AssertionError(f"Todo overview omitted distinct workflow IDs: {workflow_ids!r}\n{overview}")

        tmux("send-keys", "-t", f"{session}:0", "Enter")
        main_detail = wait_for_all(
            session,
            [
                "Main session",
                "Goal Zig Research",
                "analyze active goal Zig",
                "task (task) · completed",
            ],
        )
        (evidence / "todo-detail-main.txt").write_text(main_detail, encoding="utf-8")
        tmux("send-keys", "-t", f"{session}:0", "Escape")
        time.sleep(0.15)

        details: list[str] = []
        sorted_workflows = sorted(
            [
                ("moonbit-agent", workflow_ids[2]),
                ("zig-agent", workflow_ids[1]),
                (MOONBIT_WORKFLOW_OBJECTIVE, workflow_ids[3]),
                (ZIG_WORKFLOW_OBJECTIVE, workflow_ids[0]),
            ]
        )
        phase_by_name = {
            "moonbit-agent": "Moonbit Research",
            "zig-agent": "Zig Research",
            MOONBIT_WORKFLOW_OBJECTIVE: "Moonbit Research",
            ZIG_WORKFLOW_OBJECTIVE: "Zig Research",
        }
        ordered = [
            (name, phase_by_name[name], workflow_id)
            for name, workflow_id in sorted_workflows
        ]
        for index, (name, phase, identity) in enumerate(ordered):
            select_todo_overview_row(session, identity)
            tmux("send-keys", "-t", f"{session}:0", "Enter")
            time.sleep(0.15)
            language = "Moonbit" if "moonbit" in name else "Zig"
            needles = [
                "worker · completed",
                identity,
                f"worker research {language} workflow {identity}",
            ]
            detail = wait_for_all(session, needles)
            details.append(detail)
            (evidence / f"todo-detail-{index}.txt").write_text(detail, encoding="utf-8")
            tmux("send-keys", "-t", f"{session}:0", "Escape")
            time.sleep(0.15)

        tmux("send-keys", "-t", f"{session}:0", "Escape")
        time.sleep(0.2)
        final_capture = capture(session, history=True)
        (evidence / "tui.txt").write_text(final_capture, encoding="utf-8")
        with (evidence / "provider-events.jsonl").open("w", encoding="utf-8") as handle:
            for event in STATE.events:
                handle.write(json.dumps(event, ensure_ascii=False, separators=(",", ":")) + "\n")

        checks = [
            "exact-chinese-commands",
            "objective-only-default-name",
            "four-distinct-workflow-ids",
            "zig-workflows-preserved-after-moonbit",
            "real-goal-and-workflow-todo-tool-results",
            "request-routed-real-worker-completions",
            "todo-overview-main-plus-four-workflows",
            "todo-details-phases-tasks-linked-jobs",
            "workflow-owned-linked-jobs",
            "loopback-openai-provider",
            "bounded-waits",
            "isolated-git-workspace",
        ]
        summary = {
            "status": "passed",
            "checks": checks,
            "commands": COMMANDS,
            "workflowIds": workflow_ids,
            "workflowObjectives": objectives,
            "provider": f"http://127.0.0.1:{port}",
            "workflowTodoResults": len(STATE.workflow_todo_results),
            "goalTodoResult": STATE.goal_todo_result is not None,
            "workerCompletions": sum(STATE.worker_completions.values()),
        }
        (evidence / "assertions.json").write_text(
            json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
    finally:
        try:
            (evidence / "provider-requests.json").write_text(
                json.dumps(STATE.requests, ensure_ascii=False, indent=2) + "\n",
                encoding="utf-8",
            )
            if STATE.last_unclassified is not None:
                (evidence / "last-unclassified-request.json").write_text(
                    json.dumps(STATE.last_unclassified, ensure_ascii=False, indent=2) + "\n",
                    encoding="utf-8",
                )
            (evidence / "failure-pane.txt").write_text(
                capture(session, history=True), encoding="utf-8"
            )
        except BaseException:
            pass
        tmux("kill-session", "-t", session, check=False)
        server.shutdown()
        thread.join(timeout=5)
        server.server_close()

    print(json.dumps({"status": "passed", "evidence": str(evidence)}, ensure_ascii=False))


if __name__ == "__main__":
    main()
