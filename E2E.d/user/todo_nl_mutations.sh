#!/usr/bin/env bash
# Natural-language Todo mutation lane: NL prompts drive real Application turns
# whose model emits TYPED `todo` tool calls through the production registry
# (no client NL parser, no slash translation), with exact persisted-journal
# proof and failed-rm invariance. Mock mode routes by exact prompt text.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
. "$E2E_DIR/lib/common.sh"

SCENARIO="todo-nl-mutations"

PROMPT_ADD='todo nl: add a task'
PROMPT_REMOVE='todo nl: remove the added task'
PROMPT_MISSING='todo nl: remove the missing task'
PROMPT_CLEAR='todo nl: clear the list'
TASK_CONTENT='send the welcome email'
MISSING_TASK='never created task 6f21'

wait_for() {
    local needle="$1" timeout="${2:-25}" name="${3:-live.txt}"
    if ! tmux_wait_for "$session" "$timeout" "$needle" >"$evidence/$name"; then
        fail "user.$SCENARIO: did not find ${needle@Q} (see $evidence/$name)"
    fi
}

send_cmd() {
    tmux send-keys -t "$session":0 -l "$1"
    tmux send-keys -t "$session":0 Enter
}

start_mock() {
    local root="$1" port_file="$root/mock-port.txt" deadline port
    python3 "$E2E_DIR/lib/todo_nl_mock_server.py" --port-file "$port_file" \
        >"$evidence/mock-server.log" 2>&1 &
    register_pid $!
    deadline=$((SECONDS + 15))
    while [ ! -s "$port_file" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -s "$port_file" ] || fail "user.$SCENARIO: mock server did not write its port file"
    port="$(cat "$port_file")"
    printf '%s\n' "$port"
}

verify_journal() {
    python3 - "$1" "$2" "$SCENARIO" \
        "$PROMPT_ADD" "$PROMPT_REMOVE" "$PROMPT_MISSING" "$PROMPT_CLEAR" \
        "$TASK_CONTENT" "$MISSING_TASK" <<'PY'
from pathlib import Path
import json, sys

agent = Path(sys.argv[1])
evidence = Path(sys.argv[2])
scenario = sys.argv[3]
prompts = sys.argv[4:8]
task_content = sys.argv[8]
missing_task = sys.argv[9]

rows = []
root = agent / "sessions"
for path in sorted(root.rglob("*.jsonl")) if root.is_dir() else []:
    for line in path.read_text(encoding="utf-8").splitlines():
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            pass

def texts(row):
    message = row.get("message") if isinstance(row, dict) else None
    if not isinstance(message, dict):
        return ""
    return "\n".join(
        block.get("text", "")
        for block in message.get("content", [])
        if isinstance(block, dict) and isinstance(block.get("text"), str)
    )

user_rows = [row for row in rows if isinstance(row, dict)
             and isinstance(row.get("message"), dict)
             and row["message"].get("role") == "user"]
user_texts = [texts(row) for row in user_rows]
for prompt in prompts:
    if not any(text == prompt for text in user_texts):
        raise SystemExit(f"{scenario}: natural-language prompt not persisted verbatim: {prompt!r}")
if any(text.lstrip().startswith("/todo") for text in user_texts):
    raise SystemExit(f"{scenario}: slash-translated todo command found in the journal")

def calls(row):
    message = row.get("message") if isinstance(row, dict) else None
    if not isinstance(message, dict) or message.get("role") != "assistant":
        return []
    found = []
    for block in message.get("content", []):
        if not isinstance(block, dict) or block.get("type") != "toolCall":
            continue
        call = block.get("toolCall") if isinstance(block.get("toolCall"), dict) else block
        if isinstance(call, dict) and call.get("name") == "todo":
            try:
                arguments = json.loads(call.get("arguments", "{}"))
            except (json.JSONDecodeError, TypeError):
                arguments = {}
            found.append(arguments)
    return found

todo_calls = [arguments for row in rows for arguments in calls(row)]
ops = [arguments.get("op") for arguments in todo_calls]
expected_ops = ["append", "rm", "append", "rm", "rm"]
if ops != expected_ops:
    raise SystemExit(f"{scenario}: typed todo ops {ops} != {expected_ops}")
if todo_calls[0].get("items") != [task_content] or todo_calls[0].get("phase") != "Delivery":
    raise SystemExit(f"{scenario}: add-task arguments not the typed append: {todo_calls[0]}")
if todo_calls[1].get("task") != task_content:
    raise SystemExit(f"{scenario}: remove task arguments not typed by content")
if todo_calls[2].get("items") != [task_content]:
    raise SystemExit(f"{scenario}: re-add arguments not the typed append")
if todo_calls[3].get("task") != missing_task:
    raise SystemExit(f"{scenario}: failed rm did not target the missing task")
if todo_calls[4].get("task") not in (None, "") or todo_calls[4].get("phase") not in (None, ""):
    raise SystemExit(f"{scenario}: clear must be a bare rm, got {todo_calls[4]}")

def snapshots(row):
    return row.get("state") if isinstance(row, dict) and row.get("type") == "todo_snapshot" else None

todo_call_rows = [rows.index(row) for row in rows if isinstance(row, dict)
                  and isinstance(row.get("message"), dict) and row["message"].get("role") == "assistant"
                  and any(isinstance(block, dict) and block.get("type") == "toolCall"
                          and (block.get("toolCall") if isinstance(block.get("toolCall"), dict) else block).get("name") == "todo"
                          for block in row["message"].get("content", []))]
failed_call_row = todo_call_rows[3]
clear_call_row = todo_call_rows[4]
snapshots_between = [row for row in rows[failed_call_row + 1 : clear_call_row]
                     if isinstance(row, dict) and snapshots(row) is not None]
if snapshots_between:
    raise SystemExit(f"{scenario}: failed rm appended a durable snapshot before the clear")

todo_snapshots = [state for row in rows if (state := snapshots(row)) is not None]
if len(todo_snapshots) != 4:
    raise SystemExit(f"{scenario}: expected 4 persisted todo snapshots (failed rm persists none), got {len(todo_snapshots)}")

def task_contents(state):
    return [task.get("content") for phase in state.get("phases", []) for task in phase.get("tasks", [])]

if task_contents(todo_snapshots[0]) != [task_content]:
    raise SystemExit(f"{scenario}: first snapshot must contain exactly the added task")
if task_contents(todo_snapshots[1]) != []:
    raise SystemExit(f"{scenario}: remove mutation must persist an empty task list")
if task_contents(todo_snapshots[2]) != [task_content]:
    raise SystemExit(f"{scenario}: re-added task must persist again")
if task_contents(todo_snapshots[3]) != []:
    raise SystemExit(f"{scenario}: clear must persist an empty task list")
def failed_results(row):
    message = row.get("message") if isinstance(row, dict) else None
    if not isinstance(message, dict) or message.get("role") != "toolResult":
        return []
    text = texts(row)
    return [text] if "Errors:" in text else []

errors = [text for row in rows for text in failed_results(row)]
if not any(missing_task in text for text in errors):
    raise SystemExit(f"{scenario}: failed rm did not surface the typed domain error")

(evidence / "todo-nl-journal.json").write_text(json.dumps({
    "scenario": scenario,
    "promptsPersistedVerbatim": True,
    "typedTodoOps": ops,
    "todoSnapshotCount": len(todo_snapshots),
    "failedRmPersistedNothing": True,
    "clientSlashTranslation": False,
}, indent=2) + "\n", encoding="utf-8")
PY
}

run_mock_lane() {
    require_rpi
    require_cmd tmux
    require_cmd python3

    local root port
    root="$(scenario_workspace "$SCENARIO")"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    session="$(unique_tmux_name "$SCENARIO")"
    register_tmux_session "$session"
    mkdir -p "$root/workspace" "$root/home/.pi/agent" "$evidence"

    port="$(start_mock "$root")"
    cat >"$root/home/.pi/agent/models.json" <<EOF
{
  "providers": {
    "todo-nl": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "Todo NL Mock", "contextWindow": 32768, "maxTokens": 2048 }
      ]
    }
  }
}
EOF

    tmux new-session -d -s "$session" -x 150 -y 44 -c "$root/workspace" \
        "env HOME='$root/home' USERPROFILE='$root/home' \
PI_CODING_AGENT_DIR='$root/home/.pi/agent' \
RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 \
TERM=xterm-256color \
'$RPI_BIN' --offline --model todo-nl/mock --api-key todo-nl-mock-key; printf '===TUI-DONE-todo-nl-mutations===\n'"

    wait_for 'todo-nl/mock' 30 boot.txt
    tmux send-keys -t "$session":0 Escape

    send_cmd "$PROMPT_ADD"
    wait_for "$TASK_CONTENT" 30 add.txt
    wait_for 'Overall: 0/1 done, 1 open.' 30 add-counts.txt

    send_cmd "$PROMPT_REMOVE"
    wait_for 'Todo list cleared.' 30 remove.txt

    send_cmd "$PROMPT_ADD"
    wait_for 'Overall: 0/1 done, 1 open.' 30 re-add.txt

    send_cmd "$PROMPT_MISSING"
    wait_for 'not found by ID or content' 30 missing.txt

    send_cmd "$PROMPT_CLEAR"
    wait_for 'Todo list cleared.' 30 clear.txt

    verify_journal "$root/home/.pi/agent" "$evidence"

    tmux capture-pane -p -S -3000 -t "$session":0 >"$evidence/tui.txt" 2>/dev/null || true
    tmux send-keys -t "$session":0 -l '/quit'
    tmux send-keys -t "$session":0 Enter
    if ! tmux_wait_for "$session" 12 '===TUI-DONE-todo-nl-mutations===' >"$evidence/quit.txt"; then
        log "user.$SCENARIO: quit marker not seen; scenario assertions already passed"
    fi
    tmux kill-session -t "$session" 2>/dev/null || true
    printf 'user.%s passed\nevidence=%s\n' "$SCENARIO" "$evidence"
}

run_live_lane() {
    require_rpi
    require_cmd tmux
    require_cmd python3
    [ -n "${RPI_LIVE_MODEL:-}" ] || fail "RPI_LIVE_MODEL is required (provider/model)"
    [ -n "${RPI_LIVE_API_KEY_ENV:-}" ] || fail "RPI_LIVE_API_KEY_ENV is required (name only)"
    case "$RPI_LIVE_API_KEY_ENV" in *[!A-Za-z0-9_]*|'') fail "RPI_LIVE_API_KEY_ENV must be an environment variable name" ;; esac
    [ -n "${!RPI_LIVE_API_KEY_ENV:-}" ] || fail "required model credential environment variable is unset: $RPI_LIVE_API_KEY_ENV"

    local scenario="todo-nl-mutations-live" root evidence session credential child_id
    root="$WORK_ROOT/$scenario"
    evidence="$EVIDENCE_ROOT/$scenario"
    rm -rf "$root"
    mkdir -p "$root/workspace" "$root/home/.pi/agent" "$evidence"
    cat >"$root/home/.pi/agent/settings.json" <<'EOF'
{"defaultProjectTrust":"always","orchestration":{"process":true,"tasks":true,"todo":true,"maxConcurrency":4,"maxRecursionDepth":2}}
EOF
    credential="${!RPI_LIVE_API_KEY_ENV}"
    child_id="$(python3 -c 'import uuid; print(uuid.uuid4())')"

    session="$(unique_tmux_name "$scenario")"
    register_tmux_session "$session"
    tmux new-session -d -s "$session" -x 150 -y 44 -c "$root/workspace" \
        "env -i HOME='$root/home' USERPROFILE='$root/home' PI_CODING_AGENT_DIR='$root/home/.pi/agent' PATH='$PATH' TERM=xterm-256color '$RPI_LIVE_API_KEY_ENV'='$credential' bash -lc 'TERM=xterm-256color \"$RPI_BIN\" --approve -C \"$root/workspace\" --model \"$RPI_LIVE_MODEL\"; printf \"===TUI-DONE-todo-nl-live===\n\"'"
    wait_for 'Model' 45 boot-live.txt
    tmux send-keys -t "$session":0 Escape
    send_cmd "Start durable Todo child work: send the message 'start long work 6f21' to the durable Todo child with childSessionId exactly $child_id, ensuring that child exists, then reply NL_CHILD_SENT_6F21."
    wait_for 'NL_CHILD_SENT_6F21' 240 live-child-sent.txt
    send_cmd "List the durable Todo children now and confirm the child $child_id appears in the list, then reply NL_CHILD_LISTED_6F21."
    wait_for 'NL_CHILD_LISTED_6F21' 240 live-child-listed.txt
    send_cmd "Use the todo tool: append a task 'send the welcome email' to a phase named Delivery. Make exactly one todo tool call this turn, then reply NL_ADD_OK_6F21."
    wait_for 'NL_ADD_OK_6F21' 240 live-add.txt
    send_cmd "Use the todo tool: remove the task 'send the welcome email' from the todo list. Make exactly one todo tool call this turn, then reply NL_REMOVE_OK_6F21."
    wait_for 'NL_REMOVE_OK_6F21' 240 live-remove.txt
    send_cmd "Use the todo tool: append a task 'send the welcome email' to the Delivery phase. Make exactly one todo tool call this turn, then reply NL_READD_OK_6F21."
    wait_for 'NL_READD_OK_6F21' 240 live-readd.txt
    send_cmd "Use the todo tool: remove the todo task 'never created task 6f21'. It will fail; do not retry it. Make exactly one todo tool call this turn, report the error text, then reply NL_MISSING_REPORTED_6F21."
    wait_for 'NL_MISSING_REPORTED_6F21' 240 live-missing.txt
    send_cmd "Use the todo tool to clear the whole todo list. Make exactly one todo tool call this turn, then reply NL_CLEAR_OK_6F21."
    wait_for 'NL_CLEAR_OK_6F21' 240 live-clear.txt
    send_cmd "Cancel the durable Todo child $child_id's run now, then reply NL_CANCEL_OK_6F21."
    wait_for 'NL_CANCEL_OK_6F21' 240 live-cancel.txt
    send_cmd "Delete the durable Todo child $child_id completely now, then reply NL_DELETE_OK_6F21."
    wait_for 'NL_DELETE_OK_6F21' 240 live-delete.txt
    send_cmd "List the durable Todo children now, then reply NL_LIST_EMPTY_6F21."
    wait_for 'NL_LIST_EMPTY_6F21' 240 live-list-empty.txt

    tmux send-keys -t "$session":0 -l '/quit'
    tmux send-keys -t "$session":0 Enter
    tmux_wait_for "$session" 15 '===TUI-DONE-todo-nl-live===' >"$evidence/quit.txt" || true
    tmux kill-session -t "$session" 2>/dev/null || true

    printf '%s\n' "$child_id" >"$evidence/child-id.txt"
    python3 "$E2E_DIR/live/verify.py" todo-nl-mutations \
        "$root/home" "$root/workspace" "$evidence"
    printf 'user.%s passed\nevidence=%s\n' "$scenario" "$evidence"
}

case "${1:-run}" in
    run) run_mock_lane ;;
    --live) run_live_lane ;;
    list) printf 'user.todo-nl-mutations [run|--live]\n' ;;
    *) printf 'usage: %s [run|--live|list]\n' "$0" >&2; exit 2 ;;
esac