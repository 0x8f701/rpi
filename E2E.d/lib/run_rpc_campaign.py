#!/usr/bin/env python3
import argparse
import json
import os
import subprocess
import time
from pathlib import Path


def scenario_commands(scenario: str, workspace: str) -> list[dict]:
    groups = {
        "models": [{"type": "get_available_models", "id": "models"}],
        "tools": [
            {"type": "get_commands", "id": "commands"},
            {"type": "bash", "id": "bash", "command": "printf tools-campaign"},
        ],
        "todo": [
            {"type": "set_todos", "id": "todos", "phases": [{"name": "Release", "tasks": [
                {"id": "inventory", "content": "verify inventory", "status": "in_progress"},
                {"id": "install", "content": "verify install", "status": "pending", "dependsOn": ["inventory"]},
            ]}]},
            {"type": "get_state", "id": "todo-state"},
        ],
        "goal": [
            {"type": "goal_create", "id": "goal", "objective": "deterministic release readiness", "tokenBudget": 1000},
            {"type": "goal_get", "id": "goal-get"},
        ],
        "loop": [
            {"type": "loop_create", "id": "loop", "interval": "1h", "prompt": "deterministic scheduled check", "fireImmediately": False, "durable": False},
            {"type": "loop_list", "id": "loops"},
        ],
        "process": [
            {"type": "process_spawn", "id": "spawn", "spec": {"argv": ["sh", "-c", "printf process-ok"], "cwd": workspace, "env": {}, "tty": False, "timeoutMs": 5000}},
            {"type": "process_list", "id": "process-list"},
        ],
        "session": [
            {"type": "set_session_name", "id": "name", "name": "ci-deterministic"},
            {"type": "get_state", "id": "state"},
            {"type": "get_tree", "id": "tree"},
        ],
    }
    if scenario != "all":
        return groups[scenario]
    order = ["models", "tools", "todo", "goal", "loop", "process", "session"]
    return [command for group in order for command in groups[group]]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rpi", required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--stderr", required=True)
    parser.add_argument("--scenario", choices=["all", "models", "tools", "todo", "goal", "loop", "process", "session"], default="all")
    args = parser.parse_args()
    commands = scenario_commands(args.scenario, args.workspace)
    env = {
        "HOME": args.home,
        "USERPROFILE": args.home,
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "LANG": os.environ.get("LANG", "C.UTF-8"),
        "LC_ALL": os.environ.get("LC_ALL", "C.UTF-8"),
        "PI_CODING_AGENT_DIR": str(Path(args.home) / ".pi" / "agent"),
        "PI_OFFLINE": "1",
        "PI_SKIP_VERSION_CHECK": "1",
        "PI_FAUX_RESPONSE": "deterministic-e2e-reply",
    }
    with open(args.stderr, "wb") as stderr, open(args.output, "w", encoding="utf-8") as output:
        process = subprocess.Popen(
            [args.rpi, "--offline", "-C", args.workspace, "--model", "faux/faux-1", "--mode", "rpc"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr,
            text=True,
            bufsize=1,
            env=env,
        )
        assert process.stdin is not None and process.stdout is not None
        for command in commands:
            deadline = time.monotonic() + 30
            process.stdin.write(json.dumps(command, separators=(",", ":")) + "\n")
            process.stdin.flush()
            while True:
                if time.monotonic() >= deadline:
                    process.kill()
                    raise SystemExit(f"timed out waiting for RPC response {command['id']}")
                line = process.stdout.readline()
                if not line:
                    raise SystemExit(f"RPC stdout closed waiting for {command['id']}")
                output.write(line)
                output.flush()
                row = json.loads(line)
                if row.get("type") == "response" and row.get("id") == command["id"]:
                    if row.get("success") is not True:
                        raise SystemExit(f"RPC {command['id']} failed: {row.get('error')}")
                    break
        process.stdin.close()
        for line in process.stdout:
            output.write(line)
        code = process.wait(timeout=10)
        if code != 0:
            raise SystemExit(f"rpi RPC exited {code}")


if __name__ == "__main__":
    main()
