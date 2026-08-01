#!/usr/bin/env python3
"""Exercise rpi against a strict OpenAI-compatible mock and validate tool schemas."""

from __future__ import annotations

import argparse
import json
import os
import secrets
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from typing import Any


def validate_object_schema(schema: dict[str, Any], name: str) -> None:
    if schema.get("type") != "object":
        raise AssertionError(f"{name} schema root must be an object: {schema!r}")
    if schema.get("additionalProperties") is not False:
        raise AssertionError(f"{name} schema must reject additional properties: {schema!r}")
    properties = schema.get("properties")
    required = schema.get("required")
    if not isinstance(properties, dict) or not isinstance(required, list):
        raise AssertionError(f"{name} schema must expose properties and required: {schema!r}")
    if set(required) != set(properties):
        raise AssertionError(f"{name} required fields must match all properties: {schema!r}")


def validate_request(body: dict[str, Any]) -> None:
    tools = body.get("tools")
    if not isinstance(tools, list):
        raise AssertionError(f"OpenAI request omitted tools: {body!r}")
    functions = {
        item.get("function", {}).get("name"): item.get("function", {})
        for item in tools
        if item.get("type") == "function"
    }
    todo = functions.get("todo")
    task = functions.get("task")
    if not isinstance(todo, dict) or not isinstance(task, dict):
        raise AssertionError(f"OpenAI request omitted todo or task: {sorted(functions)!r}")
    if todo.get("strict") is not True:
        raise AssertionError(f"todo must use strict OpenAI sampling: {todo!r}")
    validate_object_schema(todo.get("parameters", {}), "todo")
    validate_object_schema(task.get("parameters", {}), "task")
    task_items = task["parameters"]["properties"]["tasks"].get("items")
    if not isinstance(task_items, dict):
        raise AssertionError(f"task batch items schema is missing: {task!r}")
    validate_object_schema(task_items, "task item")


class SchemaHandler(BaseHTTPRequestHandler):
    request_body: dict[str, Any] | None = None
    request_error: BaseException | None = None
    request_event = threading.Event()

    def do_POST(self) -> None:
        try:
            length = int(self.headers.get("content-length", "0"))
            body = json.loads(self.rfile.read(length))
            validate_request(body)
            type(self).request_body = body
        except BaseException as error:
            type(self).request_error = error
        finally:
            type(self).request_event.set()

        stream = (
            'data: {"id":"schema-e2e","model":"strict","choices":[{"index":0,"delta":{"content":"schema-ok"}}]}\n\n'
            'data: {"id":"schema-e2e","model":"strict","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}\n\n'
            "data: [DONE]\n\n"
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(stream)))
        self.end_headers()
        self.wfile.write(stream)

    def log_message(self, _format: str, *_args: object) -> None:
        return


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rpi", required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--stderr", required=True)
    args = parser.parse_args()

    home = Path(args.home)
    agent_dir = home / ".pi" / "agent"
    agent_dir.mkdir(parents=True, exist_ok=True)
    Path(args.workspace).mkdir(parents=True, exist_ok=True)

    server = HTTPServer(("localhost", 0), SchemaHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    port = server.server_address[1]

    (agent_dir / "settings.json").write_text(
        json.dumps({"orchestration": {"tasks": True, "todo": True, "maxConcurrency": 2}}),
        encoding="utf-8",
    )
    (agent_dir / "models.json").write_text(
        json.dumps(
            {
                "providers": {
                    "schema-e2e": {
                        "baseUrl": f"http://localhost:{port}",
                        "api": "openai-completions",
                        "compat": {"supportsStrictMode": True},
                        "models": [
                            {
                                "id": "strict",
                                "name": "Strict Schema",
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

    environment = {
        "HOME": str(home),
        "USERPROFILE": str(home),
        "PATH": os.environ.get("PATH", ""),
        "LANG": os.environ.get("LANG", "C.UTF-8"),
        "LC_ALL": os.environ.get("LC_ALL", "C.UTF-8"),
        "PI_CODING_AGENT_DIR": str(agent_dir),
        "PI_SKIP_VERSION_CHECK": "1",
    }
    command = [
        args.rpi,
        "-C",
        args.workspace,
        "--model",
        "schema-e2e/strict",
        "--api-key",
        secrets.token_urlsafe(24),
        "-p",
        "Validate the advertised tool schemas and answer briefly.",
    ]
    try:
        with open(args.output, "w", encoding="utf-8") as output, open(
            args.stderr, "w", encoding="utf-8"
        ) as stderr:
            completed = subprocess.run(
                command,
                env=environment,
                stdout=output,
                stderr=stderr,
                text=True,
                timeout=60,
                check=False,
            )
    finally:
        server.shutdown()
        thread.join(timeout=5)
        server.server_close()

    if completed.returncode != 0:
        raise SystemExit(f"rpi exited with status {completed.returncode}")
    if not SchemaHandler.request_event.is_set():
        raise SystemExit("rpi did not send an OpenAI request")
    if SchemaHandler.request_error is not None:
        raise SchemaHandler.request_error
    if "schema-ok" not in Path(args.output).read_text(encoding="utf-8"):
        raise SystemExit("rpi did not consume the mock OpenAI response")
    print(json.dumps({"status": "passed", "checks": ["todo-strict-schema", "task-object-schema", "task-item-strict-schema"]}))


if __name__ == "__main__":
    main()
