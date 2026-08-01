#!/usr/bin/env python3
"""Deterministic multi-workflow RPC campaign driver.

Aligned to landed public wire (pi-cli workflow_rpc + pi-coding workflow domain):

  Commands (snake_case type tags, camelCase fields):
    workflow_create {name, objective}
    workflow_list {}
    workflow_get {workflowId|name}
    workflow_pause|resume|cancel|integrate|remove {workflowId}

  WorkflowWireSnapshot (camelCase):
    workflowId, name, objective, status, generation,
    worktree? (redacted basename label — NEVER absolute path),
    branch?, baseCommit?, supervisorAgentId?, supervisorJobId?,
    failure?, integration?

  Status: queued|planning|running|paused|integrating|completed|failed|cancelled|conflicted
  Ownership (domain): {workflowId, todoTaskId} camelCase
  Branch namespace (domain worktree): rpi/workflow/<workflowId>
  Events: workflow_updated | workflow_status_changed with workflowId+generation

HARD assertions only. Missing product APIs fail closed (no false pass).
"""

from __future__ import annotations

import argparse
import json
import hashlib
import os
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


STATUS_ENUM = frozenset(
    {
        "queued",
        "planning",
        "running",
        "paused",
        "integrating",
        "completed",
        "failed",
        "cancelled",
        "conflicted",
    }
)

# Domain worktree module prefix; some adapter doubles may still emit workflow/<id>.
BRANCH_PREFIXES = ("rpi/workflow/", "workflow/")

SHARED_TODO_PHASES: list[dict[str, Any]] = [
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


def write_jsonl(path: Path, rows: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        for row in rows:
            handle.write(json.dumps(row, separators=(",", ":"), ensure_ascii=True) + "\n")


def fail(message: str) -> None:
    raise SystemExit(message)


def looks_absolute_path(value: str) -> bool:
    if not value:
        return False
    if value.startswith("/") or value.startswith("\\"):
        return True
    if re.match(r"^[A-Za-z]:[\\/]", value):
        return True
    return False


def assert_no_absolute_path_leak(payload: Any, *, context: str) -> None:
    encoded = json.dumps(payload, ensure_ascii=True)
    # Mirror workflow_rpc::wire_json_leaks_absolute_path intent.
    if (
        '"/' in encoded
        or "/home/" in encoded
        or "/tmp/" in encoded
        or "/var/" in encoded
        or "\\\\Users\\\\" in encoded
    ):
        fail(f"HARD: absolute path leaked on wire ({context}): {encoded[:400]}")


def commit_file(repo: Path, relative: str, text: str, message: str) -> None:
    path = repo / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    subprocess.run(["git", "-C", str(repo), "add", "--", relative], check=True)
    subprocess.run(
        ["git", "-C", str(repo), "commit", "-q", "-m", message],
        check=True,
    )


class RpcClient:
    def __init__(
        self,
        rpi: str,
        home: str,
        workspace: str,
        output: Path,
        stderr: Path,
        timeout: float = 40.0,
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
                "PI_FAUX_RESPONSE", "deterministic-workflow-reply"
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

    def request(
        self, command: dict[str, Any], *, allow_failure: bool = False
    ) -> dict[str, Any]:
        assert self.proc.stdin is not None and self.proc.stdout is not None
        request_id = command["id"]
        self.proc.stdin.write(json.dumps(command, separators=(",", ":")) + "\n")
        self.proc.stdin.flush()
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            line = self.proc.stdout.readline()
            if not line:
                fail(f"RPC stdout closed waiting for {request_id}")
            self._output.write(line)
            self._output.flush()
            row = json.loads(line)
            self.rows.append(row)
            if row.get("type") == "response" and row.get("id") == request_id:
                if row.get("success") is not True and not allow_failure:
                    fail(f"RPC {request_id} failed: {row.get('error')}")
                return row
        fail(f"timed out waiting for RPC response {request_id}")


def snapshot_of(response: dict[str, Any]) -> dict[str, Any]:
    data = response.get("data")
    if not isinstance(data, dict):
        fail(f"response missing data object: {response!r}")
    # Create/get/pause/... return the snapshot directly.
    if "workflowId" in data:
        return data
    if isinstance(data.get("workflow"), dict) and "workflowId" in data["workflow"]:
        return data["workflow"]
    if isinstance(data.get("snapshot"), dict) and "workflowId" in data["snapshot"]:
        return data["snapshot"]
    fail(f"response missing WorkflowWireSnapshot: {response!r}")


def list_workflows(response: dict[str, Any]) -> list[dict[str, Any]]:
    data = response.get("data") or {}
    if isinstance(data, dict) and isinstance(data.get("workflows"), list):
        return data["workflows"]
    if isinstance(data, list):
        return data
    fail(f"workflow_list missing data.workflows array: {response!r}")


def require_status(snapshot: dict[str, Any], *allowed: str) -> str:
    status = str(snapshot.get("status") or "")
    if status not in STATUS_ENUM:
        fail(f"invalid status {status!r} on {snapshot!r}")
    if allowed and status not in allowed:
        fail(f"status {status!r} not in allowed {allowed}: {snapshot!r}")
    return status


def worktree_label(snapshot: dict[str, Any]) -> str:
    """Wire field is a redacted string label (basename), not a nested path object."""
    raw = snapshot.get("worktree")
    if isinstance(raw, str) and raw.strip():
        label = raw.strip()
    elif isinstance(raw, dict):
        label = str(raw.get("label") or raw.get("worktreePath") or raw.get("path") or "").strip()
    else:
        label = str(snapshot.get("worktreePath") or "").strip()
    if not label:
        fail(f"HARD: worktree label missing on snapshot: {snapshot!r}")
    if looks_absolute_path(label):
        fail(f"HARD: worktree wire label must not be absolute: {label!r}")
    return label


def branch_of(snapshot: dict[str, Any]) -> str:
    return str(snapshot.get("branch") or "").strip()


def assert_branch_namespace(branch: str, workflow_id: str) -> None:
    if not branch:
        # Branch may populate asynchronously; only enforce shape when present.
        return
    if not any(branch.startswith(prefix) for prefix in BRANCH_PREFIXES):
        fail(
            f"HARD: branch must use workflow namespace "
            f"{BRANCH_PREFIXES!r}, got {branch!r} for {workflow_id}"
        )


def supervisor_present(snapshot: dict[str, Any]) -> bool:
    if snapshot.get("supervisorAgentId") or snapshot.get("supervisorJobId"):
        return True
    supervisor = snapshot.get("supervisor")
    if isinstance(supervisor, dict) and (
        supervisor.get("name") or supervisor.get("id") or supervisor.get("agentId")
    ):
        return True
    if isinstance(supervisor, str) and supervisor.strip():
        return True
    return False


def ownership_pairs_from_snapshot(snapshot: dict[str, Any]) -> list[tuple[str, str]]:
    workflow_id = str(snapshot.get("workflowId") or "")
    if not workflow_id:
        fail(f"snapshot missing workflowId: {snapshot!r}")
    pairs: list[tuple[str, str]] = []
    for key in ("ownership", "taskOwnership", "ownedTasks"):
        raw = snapshot.get(key)
        if not isinstance(raw, list):
            continue
        for item in raw:
            if not isinstance(item, dict):
                continue
            tid = item.get("todoTaskId") or item.get("taskId")
            wid = item.get("workflowId") or workflow_id
            if tid:
                pairs.append((str(wid), str(tid)))
    for key in ("todoPhases", "phases", "todo"):
        phases = snapshot.get(key)
        if not isinstance(phases, list):
            continue
        for phase in phases:
            if not isinstance(phase, dict):
                continue
            for task in phase.get("tasks") or []:
                if isinstance(task, dict) and task.get("id"):
                    pairs.append((workflow_id, str(task["id"])))
    todos = snapshot.get("todos") or snapshot.get("tasks")
    if isinstance(todos, list):
        for task in todos:
            if isinstance(task, dict) and task.get("id"):
                pairs.append((workflow_id, str(task["id"])))
    return pairs


def ownership_pairs_from_rows(
    rows: list[dict[str, Any]], workflow_id: str
) -> list[tuple[str, str]]:
    pairs: list[tuple[str, str]] = []
    for row in rows:
        blob = json.dumps(row, ensure_ascii=True)
        if workflow_id not in blob:
            continue
        # Prefer structured ownership objects.
        stack: list[Any] = [row]
        while stack:
            cur = stack.pop()
            if isinstance(cur, dict):
                if (
                    cur.get("workflowId") == workflow_id
                    and (cur.get("todoTaskId") or cur.get("taskId"))
                ):
                    pairs.append(
                        (
                            workflow_id,
                            str(cur.get("todoTaskId") or cur.get("taskId")),
                        )
                    )
                stack.extend(cur.values())
            elif isinstance(cur, list):
                stack.extend(cur)
    return pairs


def main() -> None:
    parser = argparse.ArgumentParser(description="Multi-workflow RPC campaign")
    parser.add_argument("--rpi", required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--stderr", required=True)
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--timeout", type=float, default=45.0)
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
    summary: dict[str, Any] = {
        "ok": True,
        "checks": [],
        "execution_status": "running",
        "product_apis": "assumed_present",
        "wire": "WorkflowWireSnapshot camelCase + snake_case status",
    }

    try:
        probe = client.request(
            {"type": "workflow_list", "id": "wf-list-empty"}, allow_failure=True
        )
        if probe.get("success") is not True:
            summary["ok"] = False
            summary["execution_status"] = "blocked_missing_product_apis"
            summary["product_apis"] = "absent"
            summary["probe_error"] = probe.get("error")
            write_jsonl(evidence / "rpc-rows.jsonl", client.rows)
            (evidence / "summary.json").write_text(
                json.dumps(summary, indent=2, ensure_ascii=True) + "\n",
                encoding="utf-8",
            )
            print(
                json.dumps(
                    {
                        "status": "blocked_missing_product_apis",
                        "evidence": str(evidence),
                    },
                    ensure_ascii=True,
                )
            )
            fail(
                "workflow RPC APIs not available; campaign scripts are registered "
                "but product surface has not landed"
            )

        empty = list_workflows(probe)
        if empty:
            fail(f"expected empty workflow list at start, got {empty!r}")
        summary["checks"].append("workflow-list-empty-initial")

        # --- Concurrent create ---
        create_alpha = client.request(
            {
                "type": "workflow_create",
                "id": "wf-create-alpha",
                "name": "alpha-flow",
                "objective": "deterministic alpha workflow objective",
            }
        )
        create_beta = client.request(
            {
                "type": "workflow_create",
                "id": "wf-create-beta",
                "name": "beta-flow",
                "objective": "deterministic beta workflow objective",
            }
        )
        alpha = snapshot_of(create_alpha)
        beta = snapshot_of(create_beta)
        assert_no_absolute_path_leak(alpha, context="create alpha")
        assert_no_absolute_path_leak(beta, context="create beta")
        alpha_id = str(alpha.get("workflowId") or "")
        beta_id = str(beta.get("workflowId") or "")
        if not alpha_id or not beta_id:
            fail(f"create missing workflowId: alpha={alpha!r} beta={beta!r}")
        if alpha_id == beta_id:
            fail(f"HARD: concurrent creates must yield distinct workflowIds: {alpha_id}")
        if alpha.get("name") != "alpha-flow" or beta.get("name") != "beta-flow":
            fail(f"create name mismatch: alpha={alpha!r} beta={beta!r}")
        require_status(alpha, "queued", "planning", "running")
        require_status(beta, "queued", "planning", "running")
        if not isinstance(alpha.get("generation"), int) or alpha["generation"] < 1:
            fail(f"HARD: generation must be positive int on create: {alpha!r}")
        summary["checks"].append("two-workflows-created-concurrently")
        summary["alpha"] = {"workflowId": alpha_id, "name": alpha.get("name")}
        summary["beta"] = {"workflowId": beta_id, "name": beta.get("name")}

        listed = list_workflows(
            client.request({"type": "workflow_list", "id": "wf-list-two"})
        )
        assert_no_absolute_path_leak(listed, context="list two")
        ids = {str(item.get("workflowId") or "") for item in listed}
        names = {str(item.get("name") or "") for item in listed}
        if alpha_id not in ids or beta_id not in ids:
            fail(f"list missing created ids: {ids!r}")
        if "alpha-flow" not in names or "beta-flow" not in names:
            fail(f"list missing created names: {names!r}")
        if len(listed) < 2:
            fail(f"expected ≥2 workflows after concurrent create, got {listed!r}")
        summary["checks"].append("workflow-list-contains-both")

        # --- Separate worktrees (redacted labels + distinct identity) ---
        alpha_get = snapshot_of(
            client.request(
                {
                    "type": "workflow_get",
                    "id": "wf-get-alpha",
                    "workflowId": alpha_id,
                }
            )
        )
        beta_get = snapshot_of(
            client.request(
                {
                    "type": "workflow_get",
                    "id": "wf-get-beta",
                    "workflowId": beta_id,
                }
            )
        )
        assert_no_absolute_path_leak(alpha_get, context="get alpha")
        assert_no_absolute_path_leak(beta_get, context="get beta")
        alpha_label = worktree_label(alpha_get)
        beta_label = worktree_label(beta_get)
        if alpha_label == beta_label:
            fail(
                f"HARD: workflows must use separate worktree identities; "
                f"both redacted to {alpha_label!r}"
            )
        alpha_branch = branch_of(alpha_get)
        beta_branch = branch_of(beta_get)
        if alpha_branch and beta_branch and alpha_branch == beta_branch:
            fail(f"HARD: worktree branches must differ: {alpha_branch!r}")
        assert_branch_namespace(alpha_branch, alpha_id)
        assert_branch_namespace(beta_branch, beta_id)
        summary["checks"].append("separate-git-worktrees")
        summary["worktrees"] = {
            "alphaLabel": alpha_label,
            "betaLabel": beta_label,
            "alphaBranch": alpha_branch,
            "betaBranch": beta_branch,
            "note": "wire worktree is redacted basename only; docs use $EVIDENCE_ROOT placeholders",
        }

        # Exercise pause/resume while plain-text planning is still non-terminal.
        # Once scoped faux Todo work is armed, deterministic workers can complete
        # before a later lifecycle request reaches the process.
        paused = snapshot_of(
            client.request(
                {
                    "type": "workflow_pause",
                    "id": "wf-pause-alpha",
                    "workflowId": alpha_id,
                }
            )
        )
        require_status(paused, "paused")
        if not isinstance(paused.get("generation"), int):
            fail(f"HARD: pause must keep typed generation: {paused!r}")
        summary["checks"].append("workflow-pause")

        resumed = snapshot_of(
            client.request(
                {
                    "type": "workflow_resume",
                    "id": "wf-resume-alpha",
                    "workflowId": alpha_id,
                }
            )
        )
        require_status(resumed, "queued", "planning", "running")
        summary["checks"].append("workflow-resume")

        # --- Ownership-scoped overlapping Todo roots ---
        for wid, req_id in ((alpha_id, "wf-todos-alpha"), (beta_id, "wf-todos-beta")):
            scoped = client.request(
                {
                    "type": "set_todos",
                    "id": req_id,
                    "workflowId": wid,
                    "phases": SHARED_TODO_PHASES,
                },
                allow_failure=True,
            )
            if scoped.get("success") is not True:
                client.request(
                    {
                        "type": "workflow_set_todos",
                        "id": f"{req_id}-alt",
                        "workflowId": wid,
                        "phases": SHARED_TODO_PHASES,
                    },
                    allow_failure=True,
                )

        alpha_todo = alpha_get
        beta_todo = beta_get
        deadline = time.monotonic() + min(12.0, args.timeout)
        while time.monotonic() < deadline:
            alpha_todo = snapshot_of(
                client.request(
                    {
                        "type": "workflow_get",
                        "id": f"wf-get-alpha-poll-{int(time.monotonic())}",
                        "workflowId": alpha_id,
                    }
                )
            )
            beta_todo = snapshot_of(
                client.request(
                    {
                        "type": "workflow_get",
                        "id": f"wf-get-beta-poll-{int(time.monotonic())}",
                        "workflowId": beta_id,
                    }
                )
            )
            if ownership_pairs_from_snapshot(alpha_todo) and ownership_pairs_from_snapshot(beta_todo):
                break
            time.sleep(0.4)

        alpha_pairs = ownership_pairs_from_snapshot(alpha_todo) or ownership_pairs_from_rows(
            client.rows, alpha_id
        )
        beta_pairs = ownership_pairs_from_snapshot(beta_todo) or ownership_pairs_from_rows(
            client.rows, beta_id
        )
        if not alpha_pairs or not beta_pairs:
            fail(
                "HARD: expected ownership-scoped todo tasks (workflowId+todoTaskId) "
                f"for both workflows; alpha={alpha_pairs!r} beta={beta_pairs!r}. "
                "Product must project WorkflowTaskOwnership or workflow-scoped Todo phases."
            )
        alpha_task_ids = {tid for _, tid in alpha_pairs}
        beta_task_ids = {tid for _, tid in beta_pairs}
        # Overlapping ready roots may use shared task id strings across workflows.
        if not alpha_task_ids or not beta_task_ids:
            fail("HARD: empty todo task id sets after ownership projection")
        composite = {(wid, tid) for wid, tid in alpha_pairs + beta_pairs}
        if len(composite) < 2:
            fail(
                f"HARD: cross-workflow ownership collapsed: {composite!r}"
            )
        # Same task id on both workflows must remain distinct via composite key.
        shared_ids = alpha_task_ids & beta_task_ids
        if shared_ids:
            for tid in shared_ids:
                if (alpha_id, tid) not in composite or (beta_id, tid) not in composite:
                    fail(
                        f"HARD: shared task id {tid!r} missing composite ownership "
                        f"for both workflows: {composite!r}"
                    )
        for wid, tid in alpha_pairs:
            if wid != alpha_id:
                fail(f"HARD: alpha ownership leaked foreign workflowId {wid} task {tid}")
        for wid, tid in beta_pairs:
            if wid != beta_id:
                fail(f"HARD: beta ownership leaked foreign workflowId {wid} task {tid}")
        summary["checks"].append("independent-ready-todo-roots-overlap")
        summary["checks"].append("cross-workflow-task-ids-no-collision")
        summary["ownership"] = {
            "alphaPairs": sorted({(w, t) for w, t in alpha_pairs}),
            "betaPairs": sorted({(w, t) for w, t in beta_pairs}),
            "sharedTaskIds": sorted(shared_ids),
        }

        if not (supervisor_present(alpha_todo) and supervisor_present(beta_todo)):
            fail(
                "HARD: each workflow supervisor must start (supervisorAgentId / "
                f"depth-1 child) — alpha={supervisor_present(alpha_todo)} "
                f"beta={supervisor_present(beta_todo)}"
            )
        summary["checks"].append("supervisors-started-per-workflow")

        # IRC / workflow events carrying ownership must stay on known ids.
        known_ids = {alpha_id, beta_id}
        leaked = []
        for row in client.rows:
            row_type = str(row.get("type") or "")
            if row_type not in {
                "workflow_updated",
                "workflow_status_changed",
                "orchestration_message",
            } and "irc" not in json.dumps(row, ensure_ascii=True).lower():
                continue
            ownership = row.get("ownership")
            data = row.get("data") if isinstance(row.get("data"), dict) else {}
            if not isinstance(ownership, dict) and isinstance(data, dict):
                ownership = data.get("ownership")
            if not isinstance(ownership, dict):
                # Event-level workflowId is fine when it matches known workflows.
                wid = row.get("workflowId") or (
                    data.get("workflowId") if isinstance(data, dict) else None
                )
                if wid and wid not in known_ids:
                    leaked.append(row)
                continue
            wid = ownership.get("workflowId")
            if wid and wid not in known_ids:
                leaked.append(row)
        if leaked:
            fail(f"HARD: event/IRC ownership leakage across workflows: {leaked!r}")
        summary["checks"].append("supervisor-irc-directives-owned")

        # --- cancel ---
        create_gamma = client.request(
            {
                "type": "workflow_create",
                "id": "wf-create-gamma",
                "name": "gamma-flow",
                "objective": "deterministic gamma cancel target",
            }
        )
        gamma = snapshot_of(create_gamma)
        gamma_id = str(gamma.get("workflowId") or "")
        if not gamma_id:
            fail(f"gamma create missing workflowId: {gamma!r}")
        known_ids.add(gamma_id)
        cancelled = snapshot_of(
            client.request(
                {
                    "type": "workflow_cancel",
                    "id": "wf-cancel-gamma",
                    "workflowId": gamma_id,
                }
            )
        )
        require_status(cancelled, "cancelled")
        # Cancellation-idempotent: second cancel either stays cancelled or is a
        # no-op success; hard-fail only if status leaves cancelled without error.
        cancelled_again = client.request(
            {
                "type": "workflow_cancel",
                "id": "wf-cancel-gamma-idempotent",
                "workflowId": gamma_id,
            },
            allow_failure=True,
        )
        if cancelled_again.get("success") is True:
            require_status(snapshot_of(cancelled_again), "cancelled")
        else:
            # Fail-closed hosts may reject terminal cancel; confirm get stays cancelled.
            still = snapshot_of(
                client.request(
                    {
                        "type": "workflow_get",
                        "id": "wf-get-gamma-after-idempotent",
                        "workflowId": gamma_id,
                    }
                )
            )
            require_status(still, "cancelled")
        summary["checks"].append("workflow-cancel-idempotent")
        summary["gamma"] = {"workflowId": gamma_id, "status": "cancelled"}

        # --- Non-conflicting integration (beta) ---
        # Host may require a non-running terminal/paused state before integrate.
        # Prefer direct integrate; if rejected, pause then integrate.
        integrating = client.request(
            {
                "type": "workflow_integrate",
                "id": "wf-integrate-beta",
                "workflowId": beta_id,
            },
            allow_failure=True,
        )
        if integrating.get("success") is not True:
            client.request(
                {
                    "type": "workflow_pause",
                    "id": "wf-pause-beta-for-integrate",
                    "workflowId": beta_id,
                },
                allow_failure=True,
            )
            integrating = client.request(
                {
                    "type": "workflow_integrate",
                    "id": "wf-integrate-beta-retry",
                    "workflowId": beta_id,
                },
                allow_failure=True,
            )
        if integrating.get("success") is not True:
            fail(f"HARD: non-conflicting integrate must succeed: {integrating!r}")
        beta_integrated = snapshot_of(integrating)
        require_status(beta_integrated, "integrating", "completed")
        if beta_integrated.get("status") == "conflicted":
            fail(f"HARD: clean integrate must not be conflicted: {beta_integrated!r}")
        assert_no_absolute_path_leak(beta_integrated, context="integrate beta")
        summary["checks"].append("non-conflicting-integration")
        summary["betaIntegration"] = {
            "status": beta_integrated.get("status"),
            "integration": beta_integrated.get("integration"),
        }

        # --- Explicit conflict preserved and visible (alpha) ---
        # Build an actual two-sided conflict using the managed worktree layout;
        # this exercises production integration rather than a test-only RPC.
        common_git_dir = subprocess.run(
            ["git", "-C", args.workspace, "rev-parse", "--path-format=absolute", "--git-common-dir"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        workflow_namespace = hashlib.sha256(
            str(Path(common_git_dir).resolve()).encode()
        ).hexdigest()[:32]
        alpha_worktree = (
            Path(args.home)
            / ".pi"
            / "agent"
            / "workflow-worktrees"
            / workflow_namespace
            / "workflow-worktrees"
            / alpha_id
        )
        if not alpha_worktree.is_dir():
            fail("HARD: managed alpha worktree is missing")
        commit_file(alpha_worktree, "README.e2e", "workflow side\n", "workflow conflict side")
        commit_file(Path(args.workspace), "README.e2e", "source side\n", "source conflict side")
        conflict_req = client.request(
            {
                "type": "workflow_integrate",
                "id": "wf-integrate-alpha-conflict",
                "workflowId": alpha_id,
            },
            allow_failure=True,
        )

        alpha_conflict = snapshot_of(
            client.request(
                {
                    "type": "workflow_get",
                    "id": "wf-get-alpha-conflict",
                    "workflowId": alpha_id,
                }
            )
        )
        # If integrate moved alpha to integrating without conflict, that is not
        # enough — require durable conflicted status for this check.
        if alpha_conflict.get("status") != "conflicted":
            # Allow success payload of inject/integrate to carry conflicted snapshot.
            if conflict_req.get("success") is True:
                try:
                    maybe = snapshot_of(conflict_req)
                except SystemExit:
                    maybe = {}
                if maybe.get("status") == "conflicted":
                    alpha_conflict = maybe
        if alpha_conflict.get("status") != "conflicted":
            fail(
                "HARD: explicit conflict must be preserved as status=conflicted "
                f"on durable workflow_get; got {alpha_conflict!r}. "
                "Product must expose a conflicted path (inject or integrate clash)."
            )
        visible = (
            alpha_conflict.get("status")
            or alpha_conflict.get("integration")
            or alpha_conflict.get("failure")
            or alpha_conflict.get("conflict")
            or alpha_conflict.get("conflicts")
        )
        if not visible:
            fail(f"HARD: conflict must remain visible on snapshot: {alpha_conflict!r}")
        assert_no_absolute_path_leak(alpha_conflict, context="alpha conflict")
        summary["checks"].append("explicit-conflict-preserved-visible")
        summary["alphaConflict"] = {
            "status": alpha_conflict.get("status"),
            "integration": alpha_conflict.get("integration"),
            "failure": alpha_conflict.get("failure"),
        }

        # --- remove cancelled gamma ---
        removed = client.request(
            {
                "type": "workflow_remove",
                "id": "wf-remove-gamma",
                "workflowId": gamma_id,
            }
        )
        if removed.get("success") is not True:
            fail(f"workflow_remove failed: {removed!r}")
        after_remove = list_workflows(
            client.request({"type": "workflow_list", "id": "wf-list-after-remove"})
        )
        after_ids = {str(item.get("workflowId") or "") for item in after_remove}
        if gamma_id in after_ids:
            fail(f"HARD: removed workflow still listed: {after_ids!r}")
        summary["checks"].append("workflow-remove")

        for label, snap in (("alpha", alpha_conflict), ("beta", beta_integrated)):
            gen = snap.get("generation")
            if not isinstance(gen, int):
                fail(f"HARD: {label} generation must be int: {gen!r}")
        summary["checks"].append("generation-field-typed-when-present")

        workflow_events = [
            row
            for row in client.rows
            if row.get("type") in {"workflow_updated", "workflow_status_changed"}
        ]
        if not workflow_events:
            fail("HARD: no public workflow events were emitted")
        for event in workflow_events:
            if "workflowId" not in event:
                fail(f"HARD: workflow event missing workflowId: {event!r}")
            if not isinstance(event.get("generation"), int):
                fail(f"HARD: workflow event generation must be int: {event!r}")
            assert_no_absolute_path_leak(event, context=f"event {event.get('type')}")
        summary["checks"].append("workflow-events-generation-gated")

        raw_workflow_events = [
            row
            for row in client.rows
            if row.get("type") in {"created", "updated", "status_changed", "removed"}
        ]
        if raw_workflow_events:
            fail(f"HARD: raw workflow domain events reached RPC: {raw_workflow_events!r}")
        summary["checks"].append("workflow-events-public-wire-only")

        required = {
            "workflow-list-empty-initial",
            "two-workflows-created-concurrently",
            "workflow-list-contains-both",
            "separate-git-worktrees",
            "independent-ready-todo-roots-overlap",
            "cross-workflow-task-ids-no-collision",
            "supervisors-started-per-workflow",
            "supervisor-irc-directives-owned",
            "workflow-pause",
            "workflow-resume",
            "workflow-cancel-idempotent",
            "non-conflicting-integration",
            "explicit-conflict-preserved-visible",
            "workflow-remove",
            "generation-field-typed-when-present",
            "workflow-events-generation-gated",
            "workflow-events-public-wire-only",
        }
        missing = sorted(required - set(summary["checks"]))
        if missing:
            fail(f"workflow rpc missing checks: {missing}")

        summary["execution_status"] = "passed"
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
        print(f"workflow RPC campaign failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
