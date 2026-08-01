#!/usr/bin/env python3
import json
import sys
from pathlib import Path


def load(path: str) -> list[dict]:
    rows: list[dict] = []
    for number, line in enumerate(Path(path).read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip() or line.startswith("scenario="):
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise SystemExit(f"{path}:{number}: invalid JSON: {error}") from error
        if not isinstance(value, dict):
            raise SystemExit(f"{path}:{number}: expected JSON object")
        rows.append(value)
    return rows


def response(rows: list[dict], request_id: str) -> dict:
    matches = [row for row in rows if row.get("type") == "response" and row.get("id") == request_id]
    if len(matches) != 1:
        raise SystemExit(f"expected one response id={request_id!r}, found {len(matches)}")
    row = matches[0]
    if row.get("success") is not True:
        raise SystemExit(f"RPC request {request_id!r} failed: {row.get('error')}")
    return row


def main() -> None:
    if len(sys.argv) < 3:
        raise SystemExit("usage: assert_jsonl.py events PATH EXPECTED_TEXT | rpc PATH ID...")
    mode, path, *arguments = sys.argv[1:]
    rows = load(path)
    if mode == "events":
        if not arguments:
            raise SystemExit("events mode requires expected text")
        types = [row.get("type") for row in rows]
        if not rows or types[0] != "session" or "agent_settled" not in types:
            raise SystemExit(f"unexpected event lifecycle: {types}")
        if arguments[0] not in Path(path).read_text(encoding="utf-8"):
            raise SystemExit(f"expected text not found: {arguments[0]!r}")
        return
    if mode == "rpc":
        if not arguments:
            raise SystemExit("rpc mode requires response ids")
        for request_id in arguments:
            response(rows, request_id)
        return
    raise SystemExit(f"unknown assertion mode: {mode}")


if __name__ == "__main__":
    main()
