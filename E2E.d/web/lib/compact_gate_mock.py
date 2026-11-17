#!/usr/bin/env python3
"""Repo `sessions` mock + a deterministic summarizer gate (compact_badge lane).

Imports `E2E.d/lib/user_mock_server.py` UNCHANGED and wraps only `route`: a
request whose system prompt is the summarizer prompt is held until the driver
writes the release file (bounded). Normal turns are untouched, so a REAL page
prompt can drive the app's own post-turn threshold compaction while the
summary provider call is provably in flight; no signals and no UI mutation are
involved.

Env:
  RPI_REPO_ROOT        repo root (defaults to this file's E2E.d/.. ancestor)
  RPI_GATE_PORT_FILE   mock port file (same contract as user_mock_server)
  RPI_GATE_RELEASE     file whose creation releases ONE held summary
  RPI_GATE_HIT         written when a summary request is held (evidence)
"""
import importlib.util
import os
import sys
import time
from pathlib import Path

repo = Path(os.environ.get("RPI_REPO_ROOT") or Path(__file__).resolve().parents[3])
spec = importlib.util.spec_from_file_location(
    "user_mock_server", repo / "E2E.d/lib/user_mock_server.py"
)
ums = importlib.util.module_from_spec(spec)
sys.modules["user_mock_server"] = ums
spec.loader.exec_module(ums)

release = Path(os.environ["RPI_GATE_RELEASE"])
hit = Path(os.environ["RPI_GATE_HIT"])


def is_summary(body):
    return any(
        message.get("role") == "system"
        and ums.SUMMARIZER_SYSTEM in str(message.get("content") or "")
        for message in body.get("messages") or []
    )


original_route = ums.UserMockServer.route


def gated_route(self, request_number, body):
    if is_summary(body):
        hit.write_text(f"{time.time():.3f}\n", encoding="utf-8")
        deadline = time.time() + 120
        while not release.exists() and time.time() < deadline:
            time.sleep(0.05)
        try:
            release.unlink()
        except FileNotFoundError:
            pass
    return original_route(self, request_number, body)


ums.UserMockServer.route = gated_route
sys.argv = [
    "compact_gate_mock",
    "--scenario",
    "sessions",
    "--port-file",
    os.environ["RPI_GATE_PORT_FILE"],
]
ums.main()
