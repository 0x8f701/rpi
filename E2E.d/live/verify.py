#!/usr/bin/env python3
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

MODE = sys.argv[1]
if MODE == "todo-child-routing":
    if len(sys.argv) != 4:
        raise SystemExit("todo-child-routing verifier requires TUI and Web evidence directories")
    TUI_EVIDENCE, WEB_EVIDENCE = Path(sys.argv[2]), Path(sys.argv[3])
    HOME = WORKSPACE = EVIDENCE = AGENT = Path(".")
elif MODE == "mutate-running-receipt":
    if len(sys.argv) != 3:
        raise SystemExit("mutate-running-receipt requires one Todo child journal path")
    RECEIPT_JOURNAL = Path(sys.argv[2])
    HOME = WORKSPACE = EVIDENCE = AGENT = Path(".")
else:
    MODE, HOME, WORKSPACE, EVIDENCE = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3]), Path(sys.argv[4])
    AGENT = HOME / ".pi" / "agent"


def fail(message: str) -> None:
    raise SystemExit(message)


def load_json(path: Path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        return None

if MODE == "mutate-running-receipt":
    raw = RECEIPT_JOURNAL.read_bytes()
    lines = raw.splitlines(keepends=True)
    decoded = [json.loads(line) for line in lines]
    latest = {}
    for row in decoded:
        if row.get("customType") != "pi.todo_child.receipt":
            continue
        receipt = row.get("data", {}).get("receipt", {})
        receipt_id = receipt.get("id")
        if isinstance(receipt_id, str) and receipt_id:
            latest[receipt_id] = receipt
    running_ids = [receipt_id for receipt_id, receipt in latest.items() if receipt.get("status") == "running"]
    if len(running_ids) != 1:
        fail(f"expected exactly one active Running receipt, found {running_ids}")
    receipt_id = running_ids[0]
    matches = []
    for index, row in enumerate(decoded):
        receipt = row.get("data", {}).get("receipt", {})
        if row.get("customType") == "pi.todo_child.receipt" and receipt.get("id") == receipt_id and receipt.get("status") == "running":
            matches.append(index)
    if len(matches) != 1:
        fail(f"expected one durable Running record for {receipt_id}, found {len(matches)}")
    index = matches[0]
    line = lines[index]
    accepted_at = decoded[index]["data"]["receipt"]["acceptedAt"]
    original = str(accepted_at)
    replacement = original[:-1] + ("8" if original.endswith("9") else "9")
    field = f'"acceptedAt":{original}'.encode()
    offset = line.find(field)
    if offset < 0:
        fail("Running receipt acceptedAt bytes are missing")
    offset += len('"acceptedAt":')
    rewritten_line = line[:offset] + replacement.encode() + line[offset + len(original):]
    if len(rewritten_line) != len(line):
        fail("receipt CAS mutation changed the journal byte length")
    lines[index] = rewritten_line
    rewritten = b"".join(lines)
    if len(rewritten) != len(raw):
        fail("receipt CAS mutation changed the journal length")
    with RECEIPT_JOURNAL.open("r+b") as journal:
        journal.seek(0)
        journal.write(rewritten)
        journal.truncate()
        journal.flush()
    print(receipt_id)
    raise SystemExit(0)

if MODE == "todo-child-routing":
    tui = load_json(TUI_EVIDENCE / "tui-evidence.json")
    web = load_json(WEB_EVIDENCE / "web-evidence.json")
    if not isinstance(tui, dict) or not isinstance(web, dict):
        fail("Todo routing evidence is missing structured TUI or Web JSON")
    required_true = [
        "mainFollowUpCompletedWhileChildRunning", "mainComposerEditableWhileChildRunning",
        "sharedCwdEdit", "sameChildSend", "displaySwitchSurvived", "shutdownFailurePropagated",
    ]
    for surface, evidence in (("TUI", tui), ("Web", web)):
        if evidence.get("routeReceiptCount") != 1:
            fail(f"{surface} must expose exactly one typed route receipt")
        receipt_id = evidence.get("routeReceiptId")
        if not isinstance(receipt_id, str) or not receipt_id:
            fail(f"{surface} lacks the authoritative routed receipt identity")
        statuses = evidence.get("routeReceiptStatuses")
        if statuses != ["queued", "running", "completed"]:
            fail(f"{surface} routed receipt lifecycle is not queued/running/completed: {statuses!r}")
        if evidence.get("routeTerminalDeliveryCount") != 1:
            fail(f"{surface} routed receipt must have exactly one terminal delivery")
        if evidence.get("parentModelTurnCountForRoute") != 0:
            fail(f"{surface} admitted a parent model turn for the routed task")
        if evidence.get("parentTaskJobCount") != 0:
            fail(f"{surface} created a parent orchestration task job")
        if evidence.get("parentRoutePromptPresent") is not False:
            fail(f"{surface} polluted the parent transcript with the routed prompt")
        if evidence.get("parentTodoSnapshotCount") != 0:
            fail(f"{surface} mutated the parent Todo state")
        if not isinstance(evidence.get("providerModelIdentity"), str) or "/" not in evidence["providerModelIdentity"]:
            fail(f"{surface} lacks a process-observable provider/model identity")
        if evidence["providerModelIdentity"].lower().startswith("deepseek/"):
            fail(f"{surface} fell back to DeepSeek instead of the project-selected public model")
        for key in required_true:
            if evidence.get(key) is not True:
                fail(f"{surface} did not prove {key}")
        screenshots = evidence.get("screenshots")
        if not isinstance(screenshots, list) or not screenshots:
            fail(f"{surface} produced no inspectable screenshot evidence")
        base = TUI_EVIDENCE if surface == "TUI" else WEB_EVIDENCE
        for screenshot in screenshots:
            if not isinstance(screenshot, str) or not (base / screenshot).is_file():
                fail(f"{surface} screenshot is missing: {screenshot!r}")
        if evidence.get("pageErrors") != [] or evidence.get("consoleErrors") != []:
            fail(f"{surface} recorded browser/page errors")
    if web.get("webSocketDisconnectSurvived") is not True:
        fail("Web did not prove WebSocket disconnect survival")
    if web.get("backendDomChildIdEqual") is not True:
        fail("Web backend and DOM child identities differ")
    (WEB_EVIDENCE / "todo-child-routing-verification.json").write_text(
        json.dumps({"tui": tui, "web": web, "verified": True}, indent=2) + "\n",
        encoding="utf-8",
    )
    raise SystemExit(0)


def session_records():
    rows = []
    root = AGENT / "sessions"
    for path in sorted(root.rglob("*.jsonl")) if root.is_dir() else []:
        try:
            for line in path.read_text(encoding="utf-8").splitlines():
                try:
                    rows.append((path, json.loads(line)))
                except json.JSONDecodeError:
                    pass
        except (OSError, UnicodeDecodeError):
            pass
    return rows


def tool_calls(rows):
    calls = []
    for path, row in rows:
        message = row.get("message") if isinstance(row, dict) else None
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        for block in message.get("content", []):
            if not isinstance(block, dict) or block.get("type") != "toolCall":
                continue
            call = block.get("toolCall") if isinstance(block.get("toolCall"), dict) else block
            if isinstance(call, dict):
                calls.append((path, call))
    return calls


def tool_results(rows):
    results = []
    for path, row in rows:
        message = row.get("message") if isinstance(row, dict) else None
        if isinstance(message, dict) and message.get("role") == "toolResult":
            results.append((path, message))
    return results


def content_text(message) -> str:
    return "\n".join(
        block["text"]
        for block in message.get("content", []) if isinstance(message, dict)
        if isinstance(block, dict) and isinstance(block.get("text"), str)
    )


def write_relative_paths(pattern: str, output: str) -> None:
    paths = sorted(AGENT.rglob(pattern)) if AGENT.is_dir() else []
    (EVIDENCE / output).write_text(
        "".join(f"{path.relative_to(AGENT)}\n" for path in paths if path.is_file()),
        encoding="utf-8",
    )


rows = session_records()
calls = tool_calls(rows)
results = tool_results(rows)
write_relative_paths("sessions/**/*.jsonl", "session-files.txt")
write_relative_paths("sessions/**/orchestration-state.json", "orchestration-state-files.txt")


if MODE == "todo-nl-mutations":
    child_id = (EVIDENCE / "child-id.txt").read_text(encoding="utf-8").strip()
    if not re.fullmatch(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", child_id or ""):
        fail(f"todo-nl-mutations: evidence child id is not a valid UUID: {child_id!r}")
    required_markers = [
        "NL_CHILD_SENT_6F21",
        "NL_CHILD_LISTED_6F21",
        "NL_ADD_OK_6F21",
        "NL_REMOVE_OK_6F21",
        "NL_READD_OK_6F21",
        "NL_MISSING_REPORTED_6F21",
        "NL_CLEAR_OK_6F21",
        "NL_CANCEL_OK_6F21",
        "NL_DELETE_OK_6F21",
        "NL_LIST_EMPTY_6F21",
    ]
    rows_text = "\n".join(
        content_text(row.get("message"))
        for _, row in rows
        if isinstance(row, dict) and isinstance(row.get("message"), dict) and row["message"].get("role") == "assistant"
    )
    for marker in required_markers:
        if marker not in rows_text:
            fail(f"todo-nl-mutations: assistant turn for {marker} never settled")

    user_text = "\n".join(
        content_text(row.get("message"))
        for _, row in rows
        if isinstance(row, dict) and isinstance(row.get("message"), dict) and row["message"].get("role") == "user"
    )
    if "/todo" in user_text:
        fail("todo-nl-mutations: slash-translated Todo command reached the journal")
    for needle in ("start long work 6f21", "send the welcome email", "never created task 6f21"):
        if needle not in user_text:
            fail(f"todo-nl-mutations: natural-language prompt missing from the persisted journal: {needle}")

    events = []
    tool_result_text = {}
    for _, row in rows:
        if not isinstance(row, dict):
            continue
        message = row.get("message")
        if isinstance(message, dict) and message.get("role") == "assistant":
            for block in message.get("content", []):
                if not isinstance(block, dict) or block.get("type") != "toolCall":
                    continue
                call = block.get("toolCall") if isinstance(block.get("toolCall"), dict) else block
                if not isinstance(call, dict):
                    continue
                try:
                    arguments = json.loads(call.get("arguments", "{}")) if isinstance(call.get("arguments"), str) else (call.get("arguments") or {})
                except json.JSONDecodeError:
                    arguments = {}
                events.append({"kind": "call", "name": call.get("name", ""), "id": call.get("id", ""), "arguments": arguments})
        elif isinstance(message, dict) and message.get("role") == "toolResult":
            result_call_id = message.get("toolCallId", "")
            if result_call_id in tool_result_text:
                fail(f"todo-nl-mutations: duplicate tool result for toolCallId {result_call_id!r}")
            tool_result_text[result_call_id] = content_text(message)
            events.append({"kind": "result", "id": result_call_id})
        elif row.get("type") == "todo_snapshot" and isinstance(row.get("state"), dict):
            events.append({"kind": "snapshot", "state": row["state"]})
    task = "send the welcome email"
    missing = "never created task 6f21"
    todo_calls = [event for event in events if event["kind"] == "call" and event["name"] == "todo"]
    if [call["arguments"] for call in todo_calls] != [
        {"op": "append", "phase": "Delivery", "items": [task]},
        {"op": "rm", "task": task},
        {"op": "append", "phase": "Delivery", "items": [task]},
        {"op": "rm", "task": missing},
        {"op": "rm"},
    ]:
        fail(
            "todo-nl-mutations: raw typed todo arguments must match exactly with no extra or missing keys: "
            + repr([call["arguments"] for call in todo_calls])
        )
    snapshots = [entry["state"] for entry in events if entry.get("kind") == "snapshot"]
    if len(snapshots) != 4:
        fail(f"todo-nl-mutations: expected exactly 4 todo snapshots (failed rm persists none), got {len(snapshots)}")

    def task_contents(state):
        return [item.get("content") for phase in state.get("phases", []) for item in phase.get("tasks", [])]

    for index, expected_tasks in enumerate([[task], [], [task], []]):
        if task_contents(snapshots[index]) != expected_tasks:
            fail(f"todo-nl-mutations: snapshot {index} task list {task_contents(snapshots[index])} != {expected_tasks}")
    failed_result = tool_result_text.get(todo_calls[3]["id"], "")
    if not (missing in failed_result and "Errors:" in failed_result):
        fail("todo-nl-mutations: the failed rm did not surface the typed domain error")
    failed_call_pos = events.index(todo_calls[3])
    clear_call_pos = events.index(todo_calls[4])
    if any(event["kind"] == "snapshot" for event in events[failed_call_pos + 1 : clear_call_pos]):
        fail("todo-nl-mutations: the failed rm appended a durable snapshot before the clear")

    def child_calls(name):
        return [event for event in events if event["kind"] == "call" and event["name"] == name]

    send_calls = child_calls("todo_child_send")
    cancel_calls = child_calls("todo_child_cancel")
    remove_calls = child_calls("todo_child_remove")
    list_calls = child_calls("todo_child_list")
    if len(send_calls) != 1 or len(cancel_calls) != 1 or len(remove_calls) != 1 or len(list_calls) != 2:
        fail(f"todo-nl-mutations: expected exactly one send/cancel/remove and two list child calls, got {len(send_calls)}/{len(list_calls)}/{len(cancel_calls)}/{len(remove_calls)}")
    ordered = [
        ("send", send_calls[0]),
        ("list", list_calls[0]),
        ("cancel", cancel_calls[0]),
        ("remove", remove_calls[0]),
        ("list", list_calls[1]),
    ]
    positions = [events.index(call) for _, call in ordered]
    if positions != sorted(positions):
        fail("todo-nl-mutations: child tool call order must be exactly send(create), list, cancel, remove, list")
    for (role, call), pos in zip(ordered, positions):
        if not call["id"]:
            fail(f"todo-nl-mutations: ordered {role} call carries no toolCallId to correlate")
        following = events[pos + 1] if pos + 1 < len(events) else None
        if following is None or following.get("kind") != "result" or following.get("id") != call["id"]:
            fail(f"todo-nl-mutations: {role} call {call['id']} is not immediately followed by its own tool result correlated by toolCallId")
        result = tool_result_text.get(call["id"], "")
        if not result.strip():
            fail(f"todo-nl-mutations: {role} call {call['id']} has an empty tool result")
    if send_calls[0]["arguments"] != {"childSessionId": child_id, "message": "start long work 6f21"}:
        fail(f"todo-nl-mutations: create/send arguments must be exactly childSessionId + message, got {send_calls[0]['arguments']}")
    if cancel_calls[0]["arguments"] != {"childSessionId": child_id}:
        fail(f"todo-nl-mutations: cancel arguments must be exactly childSessionId, got {cancel_calls[0]['arguments']}")
    if remove_calls[0]["arguments"] != {"childSessionId": child_id}:
        fail(f"todo-nl-mutations: remove arguments must be exactly childSessionId, got {remove_calls[0]['arguments']}")
    for call in (list_calls[0], list_calls[1]):
        if call["arguments"] != {}:
            fail(f"todo-nl-mutations: todo_child_list must take exactly no arguments, got {call['arguments']}")
    def parsed_payload(call):
        text = tool_result_text.get(call["id"], "")
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            fail(f"todo-nl-mutations: {call['name']} tool result was not a successful JSON payload: {text[:200]!r}")

    send_payload = parsed_payload(send_calls[0])
    cancel_payload = parsed_payload(cancel_calls[0])
    remove_payload = parsed_payload(remove_calls[0])
    receipt_id = send_payload.get("id", "")
    if send_payload.get("childSessionId") != child_id or not receipt_id or send_payload.get("status") != "queued":
        fail(f"todo-nl-mutations: todo_child_send must return the direct TodoChildReceipt binding the exact child with a fresh queued receipt id, got: {json.dumps(send_payload)[:300]}")
    cancel_receipt_ids = [
        entry.get("id", "")
        for entry in (cancel_payload.get("receipts") or [])
        if isinstance(entry, dict)
    ]
    if receipt_id not in cancel_receipt_ids:
        fail(f"todo-nl-mutations: the cancel snapshot must retain the sent receipt {receipt_id!r}, got {cancel_receipt_ids}")
    listed = parsed_payload(list_calls[0])
    if not (isinstance(listed, list) and len(listed) == 1 and isinstance(listed[0], dict) and listed[0].get("childSessionId") == child_id):
        fail(f"todo-nl-mutations: pre-delete list must contain exactly the created child, got {json.dumps(listed)[:300]}")
    if remove_payload != {"removed": True, "childSessionId": child_id}:
        fail(f"todo-nl-mutations: remove payload must be exactly {{removed: true, childSessionId}}, got: {json.dumps(remove_payload)[:200]}")
    listed = parsed_payload(list_calls[0])
    parent_files = sorted(
        {
            path
            for path, row in rows
            if isinstance(row, dict)
            and isinstance(row.get("message"), dict)
            and row["message"].get("role") == "user"
            and "start long work 6f21" in content_text(row.get("message"))
        }
    )
    if len(parent_files) != 1:
        fail(f"todo-nl-mutations: expected exactly one exercised parent journal, found {len(parent_files)}")
    journal_path = parent_files[0]
    child_root = journal_path.parent / "children" / journal_path.stem
    manifest_path = child_root / "todo-manifest.json"
    transcript_path = child_root / f"{child_id}.jsonl"
    orchestration_path = child_root / "children" / child_id / "orchestration-state.json"
    if not manifest_path.is_file():
        fail(f"todo-nl-mutations: the exact exercised manifest {manifest_path} must exist after remove")
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        fail(f"todo-nl-mutations: manifest {manifest_path} is not valid JSON: {error}")
    manifest_entries = manifest.get("entries") if isinstance(manifest, dict) else None
    if not isinstance(manifest_entries, list):
        fail(f"todo-nl-mutations: manifest {manifest_path} must be a dict with an entries list")
    if any(
        isinstance(entry, dict) and entry.get("childSessionId") == child_id
        for entry in manifest_entries
    ):
        fail(f"todo-nl-mutations: manifest {manifest_path} still carries the removed child entry")
    if transcript_path.exists():
        fail(f"todo-nl-mutations: exact child transcript {transcript_path} survived delete")
    if orchestration_path.exists():
        fail(f"todo-nl-mutations: exact orchestration sidecar {orchestration_path} survived delete")
    empty_listed = parsed_payload(list_calls[1])
    if empty_listed != []:
        fail(f"todo-nl-mutations: post-delete list must parse as exactly [], got {json.dumps(empty_listed)[:200]}")
    (EVIDENCE / "todo-nl-evidence.json").write_text(json.dumps({
        "typedTodoOps": [call["arguments"].get("op") for call in todo_calls],
        "todoSnapshotCount": len(snapshots),
        "finalTodoTaskCount": len(task_contents(snapshots[-1])),
        "failedRmSurfacedTypedError": True,
        "failedRmAppendedNoSnapshot": True,
        "exercisedParentJournal": str(journal_path),
        "manifestPath": str(manifest_path),
        "childSessionId": child_id,
        "childIdentityChain": "send==cancel==remove",
        "clientSlashTranslation": False,
    }, indent=2) + "\n", encoding="utf-8")

elif MODE == "typed-irc":

    expected = {
        ("Main", "IrcAlpha", "IRC_MAIN_TO_ALPHA_6F21"),
        ("IrcAlpha", "IrcBeta", "IRC_ALPHA_TO_BETA_6F21"),
        ("IrcBeta", "IrcAlpha", "IRC_BETA_TO_ALPHA_ACK_6F21"),
        ("IrcAlpha", "Main", "IRC_ALPHA_TO_MAIN_6F21"),
    }
    state_paths = sorted((AGENT / "sessions" / "children").rglob("orchestration-state.json")) if (AGENT / "sessions" / "children").is_dir() else []
    states = [load_json(path) for path in state_paths]
    states = [state for state in states if isinstance(state, dict)]
    if len(states) != 1:
        fail(f"typed IRC requires exactly one durable orchestration state, found {len(states)}")
    state = states[0]
    messages = state.get("deliveredMessages")
    if not isinstance(messages, list):
        fail("typed IRC durable state is missing deliveredMessages")
    matched = [message for message in messages if isinstance(message, dict) and (message.get("from"), message.get("to"), message.get("body")) in expected]
    if {(message.get("from"), message.get("to"), message.get("body")) for message in matched} != expected:
        fail("typed IRC durable records do not contain the exact relay bodies")
    if any(sum(message.get("body") == body for message in matched) != 1 for _, _, body in expected):
        fail("typed IRC exact bodies must each be delivered once")
    by_body = {message["body"]: message for message in matched}
    if any(not isinstance(message.get("id"), str) or not message["id"] for message in by_body.values()):
        fail("typed IRC delivery is missing a typed message id")
    if by_body["IRC_BETA_TO_ALPHA_ACK_6F21"].get("replyTo") != by_body["IRC_ALPHA_TO_BETA_6F21"]["id"]:
        fail("typed IRC beta reply is not correlated to alpha")
    if by_body["IRC_ALPHA_TO_MAIN_6F21"].get("replyTo") != by_body["IRC_BETA_TO_ALPHA_ACK_6F21"]["id"]:
        fail("typed IRC alpha reply is not correlated to beta")
    agents = {item.get("snapshot", {}).get("id"): item.get("snapshot", {}).get("status") for item in state.get("agents", []) if isinstance(item, dict)}
    if agents.get("IrcAlpha") not in {"idle", "parked"} or agents.get("IrcBeta") not in {"idle", "parked"}:
        fail(f"typed IRC children did not settle: {agents}")
    hub_calls = [call for _, call in calls if call.get("name") == "hub"]
    hub_results = [message for _, message in results if message.get("toolName") == "hub" and not message.get("isError")]
    required_calls = [
        ("send", "IrcAlpha", "IRC_MAIN_TO_ALPHA_6F21"),
        ("send", "IrcBeta", "IRC_ALPHA_TO_BETA_6F21"),
        ("send", "IrcAlpha", "IRC_BETA_TO_ALPHA_ACK_6F21"),
        ("send", "Main", "IRC_ALPHA_TO_MAIN_6F21"),
    ]
    for op, to, message in required_calls:
        if not any(call.get("arguments", {}).get("op") == op and call.get("arguments", {}).get("to") == to and call.get("arguments", {}).get("message") == message for call in hub_calls):
            fail(f"typed IRC is missing exact hub {op} to {to}")
    if not any(call.get("arguments", {}).get("op") == "wait" and call.get("arguments", {}).get("from") == "IrcBeta" for call in hub_calls):
        fail("typed IRC alpha child is missing beta wait")
    if not any(call.get("arguments", {}).get("op") == "wait" and call.get("arguments", {}).get("from") == "IrcAlpha" for call in hub_calls):
        fail("typed IRC Main is missing alpha wait")
    if len(hub_results) < 8:
        fail("typed IRC lacks successful Hub tool-result evidence")
    (EVIDENCE / "typed-irc-records.json").write_text(json.dumps({"messages": matched, "agents": agents, "hubCalls": len(hub_calls), "hubResults": len(hub_results)}, indent=2) + "\n", encoding="utf-8")

elif MODE == "skill-routing":
    known = [call for _, call in calls if call.get("name") == "read" and call.get("arguments", {}).get("path") == "skill://route-stamp"]
    unknown = [call for _, call in calls if call.get("name") == "read" and call.get("arguments", {}).get("path") == "skill://missing-route-6f21"]
    if len(known) != 1 or len(unknown) != 1:
        fail(f"skill routing requires one known and one unknown skill read, found {len(known)} and {len(unknown)}")
    read_results = {message.get("toolCallId"): message for _, message in results if message.get("toolName") == "read"}
    known_result = read_results.get(known[0].get("id"))
    unknown_result = read_results.get(unknown[0].get("id"))
    if not isinstance(known_result, dict) or known_result.get("isError") or "ROUTE_STAMP_BODY_6F21" not in content_text(known_result):
        fail("known skill was not successfully loaded")
    unknown_text = content_text(unknown_result) if isinstance(unknown_result, dict) else ""
    if not isinstance(unknown_result, dict) or not unknown_result.get("isError") or "unknown or untrusted skill: missing-route-6f21" not in unknown_text:
        fail("unknown skill did not produce the actionable error")
    expected = "ROUTE_STAMP_BODY_6F21\nseed=SKILL_SEED_6F21\nstatus=known-skill-executed\n"
    artifact = WORKSPACE / "route-stamp.txt"
    if not artifact.is_file() or artifact.read_text(encoding="utf-8") != expected:
        fail("skill-routing artifact is missing or incorrect")
    (EVIDENCE / "route-stamp.txt").write_text(expected, encoding="utf-8")
    (EVIDENCE / "skill-tool-evidence.json").write_text(json.dumps({"knownSkill": "route-stamp", "knownReadSucceeded": True, "unknownSkill": "missing-route-6f21", "unknownError": unknown_text}, indent=2) + "\n", encoding="utf-8")

elif MODE == "parallel-workflow":
    outbound_path = WORKSPACE / "nested-workflow-rpc.jsonl"
    if not outbound_path.is_file():
        fail("parallel Workflow model-authored RPC record is missing")
    frames = []
    for line in outbound_path.read_text(encoding="utf-8").splitlines():
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            fail("parallel Workflow RPC record contains invalid JSON")
        if not isinstance(row, dict) or row.get("direction") not in {"send", "receive"} or not isinstance(row.get("frame"), dict):
            fail("parallel Workflow RPC record has an invalid typed row")
        frames.append(row)
    sent = [row["frame"] for row in frames if row["direction"] == "send"]
    control = [frame for frame in sent if str(frame.get("type", "")).startswith("workflow_")]
    if len(control) < 2 or [control[0].get("type"), control[1].get("type")] != ["workflow_create", "workflow_create"]:
        fail("parallel Workflow first two outbound control frames must both be workflow_create")
    if {control[0].get("name"), control[1].get("name")} != {"alpha-6f21", "beta-6f21"}:
        fail("parallel Workflow outbound creates have the wrong names")
    process_calls = [call for _, call in calls if call.get("name") == "process"]
    starts = [call for call in process_calls if call.get("arguments", {}).get("op") == "start" and any(str(arg).endswith("nested-workflow-client.py") for arg in call.get("arguments", {}).get("argv", []))]
    waits = [call for call in process_calls if call.get("arguments", {}).get("op") == "wait"]
    logs = [call for call in process_calls if call.get("arguments", {}).get("op") == "logs"]
    if len(starts) != 1 or not waits or not logs:
        fail("parallel Workflow Main session lacks the model-authored client process start/logs/wait evidence")
    details = load_json(WORKSPACE / "workflow-details.json")
    by_name = {item.get("name"): item for item in details.get("workflows", []) if isinstance(item, dict)} if isinstance(details, dict) else {}
    if set(by_name) != {"alpha-6f21", "beta-6f21"}:
        fail("parallel Workflow final details are missing one Workflow")
    records = []
    record_files = []
    for path in sorted((AGENT / "workflows").rglob("records/*.json")) if (AGENT / "workflows").is_dir() else []:
        value = load_json(path)
        record = value.get("record") if isinstance(value, dict) else None
        if isinstance(record, dict) and record.get("name") in by_name:
            records.append(record)
            record_files.append(str(path.relative_to(AGENT)))
    if len(records) != 2:
        fail("parallel Workflow durable store must contain exactly two target records")
    ids = {record.get("workflowId") for record in records}
    branches = {record.get("branch") for record in records}
    worktrees = {record.get("worktreeLabel") for record in records}
    supervisor_jobs = {record.get("supervisorJobId") for record in records}
    if None in ids or len(ids) != 2 or None in branches or len(branches) != 2 or None in worktrees or len(worktrees) != 2 or None in supervisor_jobs or len(supervisor_jobs) != 2:
        fail("parallel Workflow identities, branches, worktrees, or supervisor jobs are not distinct")
    intervals = {}
    ownership = {}
    for name, item in by_name.items():
        if item.get("status") not in {"completed", "conflicted"}:
            fail(f"parallel Workflow {name} did not complete: {item.get('status')}")
        jobs = [job for job in item.get("jobs", []) if isinstance(job, dict) and isinstance(job.get("todoTaskId"), str)]
        if not jobs:
            fail(f"parallel Workflow {name} has no owned child job")
        wid, generation = item.get("id"), item.get("generation")
        ownership[name] = [(wid, job.get("todoTaskId"), generation) for job in jobs]
        intervals[name] = [(job.get("startedAtMs"), job.get("finishedAtMs"), job.get("jobId")) for job in jobs if isinstance(job.get("startedAtMs"), int) and isinstance(job.get("finishedAtMs"), int)]
        if not intervals[name]:
            fail(f"parallel Workflow {name} lacks a terminal child interval")
    overlap = [(a, b) for a in intervals["alpha-6f21"] for b in intervals["beta-6f21"] if max(a[0], b[0]) < min(a[1], b[1])]
    if not overlap:
        fail(f"parallel Workflow child jobs did not strictly overlap: {intervals}")
    histories = sorted((AGENT / "workflow-artifacts").glob("*.history.json")) if (AGENT / "workflow-artifacts").is_dir() else []
    child_calls = []
    for path in histories:
        value = load_json(path)
        if not isinstance(value, list):
            continue
        for message in value:
            if not isinstance(message, dict) or message.get("role") != "assistant":
                continue
            for block in message.get("content", []):
                if isinstance(block, dict) and block.get("type") == "toolCall":
                    call = block.get("toolCall") if isinstance(block.get("toolCall"), dict) else block
                    child_calls.append(call)
    child_starts = [call for call in child_calls if call.get("name") == "process" and call.get("arguments", {}).get("op") == "start" and call.get("arguments", {}).get("argv") == ["sh", "-c", "sleep 12"]]
    child_waits = [call for call in child_calls if call.get("name") == "process" and call.get("arguments", {}).get("op") == "wait"]
    if len(child_starts) < 2 or len(child_waits) < 2:
        fail("parallel Workflow child histories lack two supervised delay sequences")
    artifacts = sorted((AGENT / "workflow-artifacts").glob("*.md")) if (AGENT / "workflow-artifacts").is_dir() else []
    alpha_artifacts = [path for path in artifacts if "ALPHA_WORKFLOW_ARTIFACT_6F21" in path.read_text(encoding="utf-8", errors="replace")]
    beta_artifacts = [path for path in artifacts if "BETA_WORKFLOW_ARTIFACT_6F21" in path.read_text(encoding="utf-8", errors="replace")]
    if len(alpha_artifacts) != 1 or len(beta_artifacts) != 1 or alpha_artifacts[0] == beta_artifacts[0]:
        fail("parallel Workflow runtime artifacts are missing or collapsed")
    if "BETA_WORKFLOW_ARTIFACT_6F21" in alpha_artifacts[0].read_text(encoding="utf-8", errors="replace") or "ALPHA_WORKFLOW_ARTIFACT_6F21" in beta_artifacts[0].read_text(encoding="utf-8", errors="replace"):
        fail("parallel Workflow artifact markers leaked across branches")
    alpha_file, beta_file = WORKSPACE / "alpha-result.txt", WORKSPACE / "beta-result.txt"
    if alpha_file.read_text(encoding="utf-8") != "ALPHA_WORKFLOW_FILE_6F21\n" or beta_file.read_text(encoding="utf-8") != "BETA_WORKFLOW_FILE_6F21\n":
        fail("parallel Workflow integrated files are missing or incorrect")
    (EVIDENCE / "nested-workflow-rpc.jsonl").write_text(outbound_path.read_text(encoding="utf-8"), encoding="utf-8")
    (EVIDENCE / "workflow-details.json").write_text(json.dumps(details, indent=2) + "\n", encoding="utf-8")
    (EVIDENCE / "workflow-records.json").write_text(json.dumps({"records": records, "recordFiles": record_files, "ownership": ownership, "overlap": overlap, "artifacts": [str(alpha_artifacts[0].relative_to(AGENT)), str(beta_artifacts[0].relative_to(AGENT))], "modelAuthoredRpc": True}, indent=2) + "\n", encoding="utf-8")

else:
    fail(f"unsupported live evidence mode: {MODE}")
