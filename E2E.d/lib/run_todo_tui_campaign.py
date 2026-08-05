#!/usr/bin/env python3
"""Exercise a dense Todo live projection, then drive the Todo DAG overview and
detail panels through the real installed-binary TUI over tmux.

Coverage:
* Seed a multi-phase Todo (uniquely named phases, decomposed tasks, mixed
  pending/in-progress markers) via bracketed paste + Enter.
* Prove the dense live projection collapses hidden work and the composer stays
  editable (existing compact-HUD assertions preserved for the shell gate).
* Open the ``/todo`` DAG overview, Enter into detail, assert phase names, task
  texts, status markers/counts, and the absence of linked child jobs in this
  faux/offline scenario (job state is asserted only where observable).
* Esc back to overview, Esc to close, and prove the composer is editable again.

Dependencies are NOT asserted here: markdown seeding carries no dependency
syntax, so readiness/edge coverage stays in the typed/RPC suites. Every wait is
hard-bounded. Composer-editable evidence is preserved at every phase boundary.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import time
from pathlib import Path


# Three uniquely named phases; the Construct phase keeps exactly seven
# in-progress (`[/]`) tasks so the dense HUD reports "Todos \u00b7 7 active", which
# the shell gate in ci/orchestration.sh asserts against tui.txt. Pending (`[ ]`)
# tasks in Survey/Ship keep the projection dense enough to collapse hidden work.
TODO_INPUT = """/todo # Survey
- [ ] map parser surface
- [ ] map renderer surface
- [ ] map terminal ownership
- [ ] map resize behavior
- [ ] map scrollback behavior
- [ ] map editor reservation
- [ ] map subprocess ownership
# Construct
- [/] repair composer repaint
- [/] close foreground stdin
- [/] disable credential prompts
- [/] bound Todo projection
- [/] bound job projection
- [/] preserve editor rows
- [/] preserve terminal history
# Ship
- [ ] verify Todo flow
- [ ] verify Goal flow
- [ ] verify Workflow flow
- [ ] verify subprocess flow
- [ ] run focused tests
- [ ] run release campaigns
- [ ] record evidence"""

PHASE_NAMES = ("Survey", "Construct", "Ship")
DETAIL_TASK_TEXTS = ("map parser surface", "repair composer repaint", "verify Todo flow")
PENDING_MARKER = "\u25cb"  # \u25cb
ACTIVE_MARKER = "\u25cf"   # \u25cf


def run_tmux(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["tmux", *args], text=True, capture_output=True, check=check)


def capture(session: str) -> str:
    """Full pane including scrollback - used for positive needle searches and
    dense-projection evidence where stale frames are harmless."""
    return run_tmux("capture-pane", "-p", "-S", "-2000", "-t", f"{session}:0").stdout


def capture_visible(session: str) -> str:
    """Current visible window only - used for panel state checks where a stale
    scrollback frame (e.g. an erased detail chrome) would otherwise satisfy a
    negative assertion."""
    return run_tmux("capture-pane", "-p", "-t", f"{session}:0").stdout


def wait_for(session: str, needle: str, timeout: float) -> str:
    deadline = time.monotonic() + timeout
    latest = ""
    while time.monotonic() < deadline:
        latest = capture(session)
        if needle in latest:
            return latest
        time.sleep(0.1)
    raise AssertionError(f"TUI did not display {needle!r}; final pane:\n{latest}")


def wait_for_pred(session: str, predicate, timeout: float, label: str) -> str:
    """Poll the *visible* pane until ``predicate`` holds. Bounded by timeout."""
    deadline = time.monotonic() + timeout
    latest = ""
    while time.monotonic() < deadline:
        latest = capture_visible(session)
        if predicate(latest):
            return latest
        time.sleep(0.1)
    raise AssertionError(f"{label} never held on the live screen; final pane:\n{latest}")


def send(session: str, *keys: str) -> None:
    run_tmux("send-keys", "-t", f"{session}:0", *keys)


def send_literal(session: str, text: str) -> None:
    run_tmux("send-keys", "-t", f"{session}:0", "-l", text)


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
    rpi = Path(args.rpi).resolve()
    agent_dir = home / ".pi" / "agent"
    agent_dir.mkdir(parents=True, exist_ok=True)
    workspace.mkdir(parents=True, exist_ok=True)
    evidence.mkdir(parents=True, exist_ok=True)
    (agent_dir / "settings.json").write_text(
        json.dumps({"orchestration": {"tasks": False, "todo": True}}), encoding="utf-8"
    )

    checks: list[str] = []
    session = f"rpi-e2e-todo-tui-{os.getpid()}"
    try:
        run_tmux(
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
            f"HOME={home}",
            f"USERPROFILE={home}",
            f"PATH={os.environ.get('PATH', '')}",
            f"LANG={os.environ.get('LANG', 'C.UTF-8')}",
            f"LC_ALL={os.environ.get('LC_ALL', 'C.UTF-8')}",
            f"PI_CODING_AGENT_DIR={agent_dir}",
            "PI_OFFLINE=1",
            "PI_SKIP_VERSION_CHECK=1",
            "PI_FAUX_RESPONSE=todo-tui-reply",
            "TERM=xterm-256color",
            str(rpi),
            "--offline",
            "--model",
            "faux/faux-1",
        )
        time.sleep(1.5)
        pane = capture(session)
        if "RPI_EXIT=" in pane:
            raise AssertionError(f"rpi exited before Todo input:\n{pane}")

        # --- Seed the multi-phase Todo via bracketed paste + Enter ----------
        (evidence / "todo-input.txt").write_text(TODO_INPUT, encoding="utf-8")
        run_tmux("set-buffer", "-b", "todo_tui", TODO_INPUT)
        run_tmux("paste-buffer", "-b", "todo_tui", "-t", f"{session}:0")
        run_tmux("delete-buffer", "-b", "todo_tui", check=False)
        time.sleep(0.3)
        send(session, "Enter")
        time.sleep(0.4)
        if "Todos \u00b7" not in capture(session):
            send(session, "Enter")

        # Dense live projection: "Todos \u00b7 7 active" + collapsed overflow marker.
        pane = wait_for(session, "Todos \u00b7 7 active", 10)
        checks.append("dense-todo-projection")
        if "more open todos" not in pane and "more active todos" not in pane:
            (evidence / "projection-failure.txt").write_text(pane, encoding="utf-8")
            raise AssertionError("dense Todo projection did not collapse hidden work")
        checks.append("collapsed-work")

        # Composer stays editable: a literal sentinel accepts a trailing char.
        sentinel = "TODO-COMPOSER-SENTINEL"
        send_literal(session, sentinel)
        wait_for(session, sentinel, 5)
        send_literal(session, "X")
        pane = wait_for(session, f"{sentinel}X", 5)
        (evidence / "tui.txt").write_text(pane, encoding="utf-8")
        checks.append("composer-editable")

        # --- Open the /todo DAG overview ------------------------------------
        # Clear the sentinel draft (Ctrl-U kills the composer line) and open the
        # overview with an empty `/todo` command.
        send(session, "C-u")
        time.sleep(0.2)
        send_literal(session, "/todo")
        time.sleep(0.2)
        send(session, "Enter")
        time.sleep(0.4)
        if "Todo DAGs" not in capture_visible(session):
            send(session, "Enter")
        overview = wait_for_pred(
            session,
            lambda live: "Todo DAGs" in live
            and ("Enter details" in live or "Main session" in live),
            12,
            "todo overview open",
        )
        (evidence / "todo-overview.txt").write_text(overview, encoding="utf-8")
        checks.append("todo-overview-opened")
        if "Main session" not in overview or "[main]" not in overview:
            raise AssertionError(f"overview missing main DAG row:\n{overview}")
        checks.append("todo-overview-main-row")
        # Overview counts row: "{exec} \u00b7 \u27130 open 21 active 7 blocked 0".
        if "open 21 active 7 blocked 0" not in overview:
            raise AssertionError(f"overview counts not projected:\n{overview}")
        checks.append("todo-overview-counts")

        # --- Enter detail and assert phases/tasks/markers/counts/jobs -------
        send(session, "Enter")
        detail = wait_for_pred(
            session,
            lambda live: "Todo DAG detail" in live
            and any(task in live for task in DETAIL_TASK_TEXTS),
            12,
            "todo detail open",
        )
        (evidence / "todo-detail.txt").write_text(detail, encoding="utf-8")
        checks.append("todo-detail-opened")

        if "Main session" not in detail or "[main]" not in detail:
            raise AssertionError(f"detail missing main DAG header:\n{detail}")
        checks.append("todo-detail-header")
        # Detail counts line: "\u2713 0 completed \u00b7 21 open \u00b7 7 active \u00b7 0 blocked".
        for fragment in ("0 completed", "21 open", "7 active", "0 blocked"):
            if fragment not in detail:
                raise AssertionError(f"detail counts missing {fragment!r}:\n{detail}")
        checks.append("todo-detail-counts")

        missing_phases = [name for name in PHASE_NAMES if name not in detail]
        if missing_phases:
            raise AssertionError(f"detail missing phase names {missing_phases}:\n{detail}")
        checks.append("todo-detail-phases")

        missing_tasks = [task for task in DETAIL_TASK_TEXTS if task not in detail]
        if missing_tasks:
            raise AssertionError(f"detail missing task texts {missing_tasks}:\n{detail}")
        checks.append("todo-detail-tasks")

        if PENDING_MARKER not in detail or ACTIVE_MARKER not in detail:
            raise AssertionError(f"detail missing status markers:\n{detail}")
        checks.append("todo-detail-status-markers")
        if "pending" not in detail or "in progress" not in detail:
            raise AssertionError(f"detail missing status text:\n{detail}")
        checks.append("todo-detail-status-text")

        # Linked child job state: with the faux model and orchestration tasks
        # disabled, no jobs are spawned, so the detail must show zero linked
        # `job:` rows. Job state is asserted only where observable.
        if "job:" in detail:
            raise AssertionError(f"detail unexpectedly linked child jobs:\n{detail}")
        checks.append("todo-detail-no-linked-jobs")

        # --- Esc back to overview, Esc to close -----------------------------
        send(session, "Escape")
        back = wait_for_pred(
            session,
            lambda live: "Todo DAGs" in live
            and "Todo DAG detail" not in live
            and ("Enter details" in live or "Main session" in live),
            10,
            "esc back to overview",
        )
        (evidence / "todo-back-overview.txt").write_text(back, encoding="utf-8")
        checks.append("todo-back-to-overview")

        send(session, "Escape")
        closed = wait_for_pred(
            session,
            lambda live: "Todo DAG" not in live and "Enter details" not in live,
            10,
            "esc close panel",
        )
        (evidence / "todo-closed.txt").write_text(closed, encoding="utf-8")
        checks.append("todo-panel-closed")

        # Composer editable again after the panel closes.
        sentinel2 = "TODO-AFTER-CLOSE"
        send_literal(session, sentinel2)
        wait_for_pred(session, lambda live: sentinel2 in live, 5, "post-close sentinel")
        send_literal(session, "Y")
        pane = wait_for_pred(
            session, lambda live: f"{sentinel2}Y" in live, 5, "post-close sentinel typed"
        )
        (evidence / "todo-after-close.txt").write_text(pane, encoding="utf-8")
        checks.append("composer-editable-after-close")

        (evidence / "assertions.json").write_text(
            json.dumps({"status": "passed", "checks": checks}),
            encoding="utf-8",
        )
    finally:
        run_tmux("kill-session", "-t", session, check=False)

    print(json.dumps({"status": "passed", "evidence": str(evidence), "checks": checks}))


if __name__ == "__main__":
    main()