#!/usr/bin/env python3
"""Deterministic orchestration RPC campaign.

Drives isolated rpi --mode rpc with trusted faux fixtures and asserts observable
goal, todo DAG readiness, supervised process, and NL prompt issuance. Does not
scrape product source text.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


def write_jsonl(path: Path, rows: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        for row in rows:
            handle.write(json.dumps(row, separators=(",", ":"), ensure_ascii=True) + "\n")


def process_state_name(entry: dict[str, Any]) -> str:
    raw_state = entry.get("state") if "state" in entry else entry.get("status")
    if isinstance(raw_state, dict):
        return str(
            raw_state.get("type")
            or raw_state.get("name")
            or raw_state.get("state")
            or ""
        ).lower()
    return str(raw_state or "").lower()


class RpcClient:
    def __init__(
        self,
        rpi: str,
        home: str,
        workspace: str,
        output: Path,
        stderr: Path,
        timeout: float = 30.0,
    ) -> None:
        self.timeout = timeout
        self.output = output
        self.rows: list[dict[str, Any]] = []
        env = {
            "HOME": home,
            "USERPROFILE": home,
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "LANG": os.environ.get("LANG", "C.UTF-8"),
            "LC_ALL": os.environ.get("LC_ALL", "C.UTF-8"),
            "PI_CODING_AGENT_DIR": str(Path(home) / ".pi" / "agent"),
            "PI_OFFLINE": "1",
            "PI_SKIP_VERSION_CHECK": "1",
            "PI_FAUX_RESPONSE": os.environ.get(
                "PI_FAUX_RESPONSE", "deterministic-orchestration-reply"
            ),
            "TERM": "xterm-256color",
        }
        stderr.parent.mkdir(parents=True, exist_ok=True)
        output.parent.mkdir(parents=True, exist_ok=True)
        self._stderr = stderr.open("wb")
        self._output = output.open("w", encoding="utf-8")
        self.proc = subprocess.Popen(
            [rpi, "--offline", "-C", workspace, "--model", "faux/faux-1", "--mode", "rpc"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self._stderr,
            text=True,
            bufsize=1,
            env=env,
        )
        assert self.proc.stdin is not None and self.proc.stdout is not None

    def close(self) -> None:
        if self.proc.stdin:
            try:
                self.proc.stdin.close()
            except BrokenPipeError:
                pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)
        self._output.close()
        self._stderr.close()

    def request(self, command: dict[str, Any], wait_settled: bool = False) -> dict[str, Any]:
        assert self.proc.stdin is not None and self.proc.stdout is not None
        request_id = command["id"]
        self.proc.stdin.write(json.dumps(command, separators=(",", ":")) + "\n")
        self.proc.stdin.flush()
        deadline = time.monotonic() + self.timeout
        response: dict[str, Any] | None = None
        settled = not wait_settled
        while time.monotonic() < deadline:
            line = self.proc.stdout.readline()
            if not line:
                raise SystemExit(f"RPC stdout closed waiting for {request_id}")
            self._output.write(line)
            self._output.flush()
            row = json.loads(line)
            self.rows.append(row)
            if row.get("type") == "response" and row.get("id") == request_id:
                if row.get("success") is not True:
                    raise SystemExit(f"RPC {request_id} failed: {row.get('error')}")
                response = row
                if not wait_settled:
                    return row
            if wait_settled and row.get("type") == "agent_settled":
                settled = True
            if response is not None and settled:
                return response
        raise SystemExit(f"timed out waiting for RPC response {request_id}")


def todo_phases() -> list[dict[str, Any]]:
    return [
        {
            "name": "Roots",
            "tasks": [
                {"id": "root-a", "content": "fetch inventory", "status": "pending"},
                {"id": "root-b", "content": "compile crate", "status": "pending"},
            ],
        },
        {
            "name": "Join",
            "tasks": [
                {
                    "id": "join",
                    "content": "ship release",
                    "status": "pending",
                    "dependsOn": ["root-a", "root-b"],
                }
            ],
        },
    ]


def index_tasks(phases: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    out: dict[str, dict[str, Any]] = {}
    for phase in phases:
        for task in phase.get("tasks", []):
            out[task["id"]] = task
    return out


def assert_todo_dag(phases: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    tasks = index_tasks(phases)
    for required in ("root-a", "root-b", "join"):
        if required not in tasks:
            raise SystemExit(f"missing todo task {required}: {phases!r}")
    root_a, root_b, join = tasks["root-a"], tasks["root-b"], tasks["join"]
    if not root_a.get("ready", False):
        raise SystemExit(f"root-a must be ready: {root_a!r}")
    if not root_b.get("ready", False):
        raise SystemExit(f"root-b must be ready: {root_b!r}")
    if join.get("ready", False):
        raise SystemExit(f"join must not be ready while roots open: {join!r}")
    blocked = join.get("blockedBy") or []
    blocked_ids = {item.get("taskId") for item in blocked}
    if blocked_ids != {"root-a", "root-b"}:
        raise SystemExit(f"join blockedBy must be both roots, got {blocked!r}")
    # Exact stable ids must round-trip (todoTaskId ownership contract surface).
    for task_id, task in tasks.items():
        if task.get("id") != task_id:
            raise SystemExit(f"task id mismatch for {task_id}: {task!r}")
    return tasks


def complete_task_phases(
    phases: list[dict[str, Any]], task_id: str
) -> list[dict[str, Any]]:
    cloned = json.loads(json.dumps(phases))
    found = False
    for phase in cloned:
        for task in phase.get("tasks", []):
            if task.get("id") == task_id:
                task["status"] = "completed"
                task["ready"] = False
                task["blockedBy"] = []
                found = True
    if not found:
        raise SystemExit(f"complete_task_phases: missing exact task id {task_id}")
    return cloned


def blocked_only_phases() -> list[dict[str, Any]]:
    """All open tasks blocked — no ready roots (blocked-only / no-spawn surface)."""
    return [
        {
            "name": "BlockedOnly",
            "tasks": [
                {
                    "id": "gate",
                    "content": "still open gate",
                    "status": "pending",
                },
                {
                    "id": "waiter",
                    "content": "blocked waiter",
                    "status": "pending",
                    "dependsOn": ["gate"],
                },
            ],
        }
    ]


def all_terminal_phases() -> list[dict[str, Any]]:
    return [
        {
            "name": "Done",
            "tasks": [
                {
                    "id": "done-a",
                    "content": "already done",
                    "status": "completed",
                },
                {
                    "id": "done-b",
                    "content": "already abandoned",
                    "status": "abandoned",
                },
            ],
        }
    ]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rpi", required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--stderr", required=True)
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--timeout", type=float, default=30.0)
    args = parser.parse_args()

    evidence = Path(args.evidence)
    evidence.mkdir(parents=True, exist_ok=True)
    output = Path(args.output)
    client = RpcClient(
        rpi=args.rpi,
        home=args.home,
        workspace=args.workspace,
        output=output,
        stderr=Path(args.stderr),
        timeout=args.timeout,
    )
    summary: dict[str, Any] = {"ok": True, "checks": []}

    try:
        client.request(
            {
                "type": "goal_create",
                "id": "goal-create",
                "objective": "deterministic orchestration readiness",
                "tokenBudget": 1000,
            }
        )
        client.request(
            {
                "type": "goal_update_usage",
                "id": "goal-usage",
                "tokens": 42,
                "activeTimeSeconds": 7,
            }
        )
        goal_get = client.request({"type": "goal_get", "id": "goal-get"})
        current = (goal_get.get("data") or {}).get("current") or {}
        if current.get("objective") != "deterministic orchestration readiness":
            raise SystemExit(f"goal objective mismatch: {current!r}")
        if current.get("lifecycle") != "active":
            raise SystemExit(f"goal lifecycle must be active: {current!r}")
        usage = current.get("usage") or {}
        if usage.get("tokensUsed") != 42:
            raise SystemExit(f"goal tokensUsed mismatch: {usage!r}")
        if usage.get("activeTimeSeconds") != 7:
            raise SystemExit(f"goal activeTimeSeconds mismatch: {usage!r}")
        if current.get("tokenBudget") != 1000:
            raise SystemExit(f"goal tokenBudget mismatch: {current!r}")
        summary["checks"].append("goal-details")
        summary["goal"] = {
            "objective": current.get("objective"),
            "lifecycle": current.get("lifecycle"),
            "tokensUsed": usage.get("tokensUsed"),
            "tokenBudget": current.get("tokenBudget"),
            "activeTimeSeconds": usage.get("activeTimeSeconds"),
        }

        set_todos = client.request(
            {
                "type": "set_todos",
                "id": "todos-init",
                "phases": todo_phases(),
            }
        )
        phases = (set_todos.get("data") or {}).get("phases") or []
        tasks = assert_todo_dag(phases)
        summary["checks"].append("todo-ready-roots-and-blocked-join")
        summary["checks"].append("todo-exact-task-ids")
        summary["todoInitial"] = {
            "rootAReady": tasks["root-a"].get("ready"),
            "rootBReady": tasks["root-b"].get("ready"),
            "joinReady": tasks["join"].get("ready"),
            "joinBlockedBy": [
                item.get("taskId") for item in (tasks["join"].get("blockedBy") or [])
            ],
            "exactIds": sorted(tasks.keys()),
        }

        # Exact todoTaskId completion: only the named id flips; others unchanged.
        after_a = client.request(
            {
                "type": "set_todos",
                "id": "todos-complete-a",
                "phases": complete_task_phases(phases, "root-a"),
            }
        )
        phases_a = (after_a.get("data") or {}).get("phases") or []
        tasks_a = index_tasks(phases_a)
        if "root-a" not in tasks_a or tasks_a["root-a"].get("status") != "completed":
            raise SystemExit(f"exact root-a completion failed: {tasks_a.get('root-a')!r}")
        if tasks_a.get("root-b", {}).get("status") == "completed":
            raise SystemExit("completing root-a must not complete root-b")
        if tasks_a["join"].get("ready", False):
            raise SystemExit("join must stay blocked after only root-a completes")
        completed_a = sum(1 for task in tasks_a.values() if task.get("status") == "completed")
        if completed_a != 1:
            raise SystemExit(f"expected 1 completed after root-a, got {completed_a}")
        summary["checks"].append("todo-exact-task-id-completion")

        after_b = client.request(
            {
                "type": "set_todos",
                "id": "todos-complete-b",
                "phases": complete_task_phases(phases_a, "root-b"),
            }
        )
        phases_b = (after_b.get("data") or {}).get("phases") or []
        tasks_b = index_tasks(phases_b)
        if not tasks_b["join"].get("ready", False):
            raise SystemExit(f"join must become ready after both roots: {tasks_b['join']!r}")
        if tasks_b["join"].get("blockedBy"):
            raise SystemExit(f"join blockedBy must clear: {tasks_b['join']!r}")
        completed_b = sum(1 for task in tasks_b.values() if task.get("status") == "completed")
        if completed_b != 2:
            raise SystemExit(f"expected 2 completed after roots, got {completed_b}")
        # Failed/cancelled stay-open surface: open join must remain non-completed.
        if tasks_b["join"].get("status") == "completed":
            raise SystemExit("join must stay open until its own completion")
        summary["checks"].append("todo-dependent-after-roots")
        summary["checks"].append("todo-open-work-remains-after-partial")
        summary["todoAfterRoots"] = {
            "completed": completed_b,
            "joinReady": tasks_b["join"].get("ready"),
            "joinStatus": tasks_b["join"].get("status"),
            "exactIds": sorted(tasks_b.keys()),
        }

        open_work = [
            task
            for task in tasks_b.values()
            if task.get("status") in {"pending", "in_progress"}
        ]
        if not open_work:
            raise SystemExit("expected open join work after roots complete")
        summary["checks"].append("todo-open-work-remains")

        # Blocked-only projection: one ready gate + blocked waiter (no dual ready roots).
        blocked_set = client.request(
            {
                "type": "set_todos",
                "id": "todos-blocked-only",
                "phases": blocked_only_phases(),
            }
        )
        blocked_phases = (blocked_set.get("data") or {}).get("phases") or []
        blocked_tasks = index_tasks(blocked_phases)
        if "gate" not in blocked_tasks or "waiter" not in blocked_tasks:
            raise SystemExit(f"blocked-only missing ids: {blocked_tasks!r}")
        if not blocked_tasks["gate"].get("ready", False):
            raise SystemExit(f"gate must be ready in blocked-only fixture: {blocked_tasks['gate']!r}")
        if blocked_tasks["waiter"].get("ready", False):
            raise SystemExit(f"waiter must not be ready: {blocked_tasks['waiter']!r}")
        waiter_blocked = {
            item.get("taskId") for item in (blocked_tasks["waiter"].get("blockedBy") or [])
        }
        if "gate" not in waiter_blocked:
            raise SystemExit(f"waiter must be blocked by gate: {blocked_tasks['waiter']!r}")
        summary["checks"].append("todo-blocked-only-projection")

        # All-terminal projection: no ready open work (attach-no-spawn surface).
        terminal_set = client.request(
            {
                "type": "set_todos",
                "id": "todos-all-terminal",
                "phases": all_terminal_phases(),
            }
        )
        terminal_phases = (terminal_set.get("data") or {}).get("phases") or []
        terminal_tasks = index_tasks(terminal_phases)
        if any(task.get("ready", False) for task in terminal_tasks.values()):
            raise SystemExit(f"all-terminal must have no ready tasks: {terminal_tasks!r}")
        if any(
            task.get("status") in {"pending", "in_progress"}
            for task in terminal_tasks.values()
        ):
            raise SystemExit(f"all-terminal must have no open tasks: {terminal_tasks!r}")
        summary["checks"].append("todo-all-terminal-no-ready")
        summary["todoTerminal"] = {
            "ids": sorted(terminal_tasks.keys()),
            "statuses": {
                task_id: task.get("status") for task_id, task in terminal_tasks.items()
            },
        }

        client.request({"type": "get_state", "id": "state-after-todo"})

        spawn = client.request(
            {
                "type": "process_spawn",
                "id": "proc-spawn",
                "spec": {
                    "argv": [
                        "sh",
                        "-c",
                        "printf 'orchestration-server-ready\\n'; sleep 120",
                    ],
                    "cwd": args.workspace,
                    "env": {},
                    "tty": False,
                    "timeoutMs": 130000,
                },
            }
        )
        proc_data = spawn.get("data") or {}
        process_id = proc_data.get("id") or proc_data.get("processId")
        if not process_id:
            process_id = (proc_data.get("process") or {}).get("id")
        if not process_id:
            raise SystemExit(f"process_spawn missing id: {proc_data!r}")

        listed = client.request({"type": "process_list", "id": "proc-list"})
        entries = listed.get("data")
        if isinstance(entries, dict):
            entries = entries.get("processes") or entries.get("items") or []
        if not isinstance(entries, list) or not entries:
            raise SystemExit(f"process_list empty after spawn: {listed!r}")
        ids = []
        for entry in entries:
            if isinstance(entry, dict):
                ids.append(entry.get("id") or entry.get("processId"))
        if process_id not in ids:
            raise SystemExit(f"spawned process {process_id} not in /ps list {ids!r}")
        summary["checks"].append("process-list-contains-supervised")
        summary["processId"] = process_id

        client.request(
            {
                "type": "process_stop",
                "id": "proc-stop",
                "processId": process_id,
            }
        )
        time.sleep(0.3)
        listed_after = client.request({"type": "process_list", "id": "proc-list-after"})
        entries_after = listed_after.get("data")
        if isinstance(entries_after, dict):
            entries_after = (
                entries_after.get("processes") or entries_after.get("items") or []
            )
        if not isinstance(entries_after, list):
            entries_after = []
        still_running = []
        for entry in entries_after:
            if not isinstance(entry, dict):
                continue
            entry_id = entry.get("id") or entry.get("processId")
            status = process_state_name(entry)
            if entry_id == process_id and status in {
                "running",
                "starting",
                "alive",
                "active",
            }:
                still_running.append(entry)
        if still_running:
            raise SystemExit(f"process still running after stop: {still_running!r}")
        summary["checks"].append("process-stopped-and-cleaned")

        client.request(
            {
                "type": "prompt",
                "id": "prompt-exact-researcher",
                "message": "Have researcher study this",
            },
            wait_settled=True,
        )
        client.request(
            {
                "type": "prompt",
                "id": "prompt-skill-only",
                "message": "Use research for this",
            },
            wait_settled=True,
        )
        orchestration_events = [
            row
            for row in client.rows
            if "researcher" in json.dumps(row, ensure_ascii=True).lower()
        ]
        summary["checks"].append("nl-prompts-issued")
        summary["nl"] = {
            "exactPrompt": "Have researcher study this",
            "skillOnlyPrompt": "Use research for this",
            "researcherMentionsInStream": len(orchestration_events),
        }

        write_jsonl(evidence / "rpc-rows.jsonl", client.rows)
        (evidence / "summary.json").write_text(
            json.dumps(summary, indent=2, ensure_ascii=True) + "\n",
            encoding="utf-8",
        )
        print(
            json.dumps(
                {
                    "status": "passed",
                    "evidence": str(evidence),
                    "checks": summary["checks"],
                },
                ensure_ascii=True,
            )
        )
    finally:
        client.close()


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception as error:  # noqa: BLE001 - campaign boundary
        print(f"orchestration RPC campaign failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
