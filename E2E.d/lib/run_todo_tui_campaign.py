#!/usr/bin/env python3
"""Exercise a dense Todo live projection and prove the composer remains editable."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import time
from pathlib import Path


TODO_INPUT = """/todo # Discovery
- [ ] inspect parser behavior
- [ ] inspect renderer behavior
- [ ] inspect terminal ownership
- [ ] inspect resize behavior
- [ ] inspect scrollback behavior
- [ ] inspect editor reservation
- [ ] inspect subprocess ownership
# Build
- [/] repair composer repaint
- [/] close foreground stdin
- [/] disable credential prompts
- [/] bound Todo projection
- [/] bound job projection
- [/] preserve editor rows
- [/] preserve terminal history
# Release
- [ ] verify Todo flow
- [ ] verify Goal flow
- [ ] verify Workflow flow
- [ ] verify subprocess flow
- [ ] run focused tests
- [ ] run release campaigns
- [ ] record evidence"""


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
    raise AssertionError(f"TUI did not display {needle!r}; final pane:\n{latest}")


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

        (evidence / "todo-input.txt").write_text(TODO_INPUT, encoding="utf-8")
        run_tmux("set-buffer", "-b", "todo_tui", TODO_INPUT)
        run_tmux("paste-buffer", "-b", "todo_tui", "-t", f"{session}:0")
        run_tmux("delete-buffer", "-b", "todo_tui", check=False)
        time.sleep(0.3)
        run_tmux("send-keys", "-t", f"{session}:0", "Enter")
        time.sleep(0.4)
        if "Todos ·" not in capture(session):
            run_tmux("send-keys", "-t", f"{session}:0", "Enter")
        pane = wait_for(session, "Todos · 7 active", 10)
        if "more open todos" not in pane and "more active todos" not in pane:
            (evidence / "projection-failure.txt").write_text(pane, encoding="utf-8")
            raise AssertionError("dense Todo projection did not collapse hidden work")

        sentinel = "TODO-COMPOSER-SENTINEL"
        run_tmux("send-keys", "-t", f"{session}:0", "-l", sentinel)
        wait_for(session, sentinel, 5)
        run_tmux("send-keys", "-t", f"{session}:0", "-l", "X")
        pane = wait_for(session, f"{sentinel}X", 5)
        (evidence / "tui.txt").write_text(pane, encoding="utf-8")
        (evidence / "assertions.json").write_text(
            json.dumps(
                {
                    "status": "passed",
                    "checks": ["dense-todo-projection", "collapsed-work", "composer-editable"],
                }
            ),
            encoding="utf-8",
        )
    finally:
        run_tmux("kill-session", "-t", session, check=False)

    print(json.dumps({"status": "passed", "evidence": str(evidence)}))


if __name__ == "__main__":
    main()
