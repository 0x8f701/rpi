#!/usr/bin/env bash
# User-perspective durable Todo child session: create and select a child, inspect
# its authoritative master-detail page, send autonomous work, add a task, send
# child-owned IRC, exercise pause/resume/stop, switch the displayed parent while
# the child remains active, and recover the same child after a process restart.
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
E2E_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck source=../lib/common.sh
. "$E2E_DIR/lib/common.sh"
# shellcheck source=../lib/orchestration_fixtures.sh
. "$E2E_DIR/lib/orchestration_fixtures.sh"

SCENARIO="todo-child-session"
PARENT_SESSION_ID="todo-parent-one"
CHILD_LABEL="durable-child"
SLOW_MESSAGE="sessions-slow-a"
TASK_CONTENT="durable task after switch"
IRC_BODY="durable child self notice"
PAUSED_MESSAGE="queued while paused"
RESTART_MESSAGE="post restart probe"

wait_for() { # needle [timeout] [evidence-name]
    local needle="$1" timeout="${2:-25}" output="${3:-live.txt}"
    if ! tmux_wait_for "$session" "$timeout" "$needle" >"$evidence/$output"; then
        fail "user.$SCENARIO: did not find ${needle@Q} (see $evidence/$output)"
    fi
}

send_cmd() { # literal command line
    tmux send-keys -t "$session":0 -l "$1"
    tmux send-keys -t "$session":0 Enter
}

OWNED_PANE_PID=""
OWNED_PROCESS_PID=""
OWNED_PROCESS_EXE=""
OWNED_PROCESS_CANDIDATES=""
SIGNALED_RPI_PID=""
OWNED_PROCESS_STARTTIME=""


process_starttime() { # pid
    python3 - "$1" <<'PY'
from pathlib import Path
import sys
fields = Path(f"/proc/{sys.argv[1]}/stat").read_text(encoding="utf-8").rsplit(") ", 1)[1].split()
print(fields[19])
PY
}

discover_owned_process() { # tmux-session executable
    local target_session="$1" target_executable="$2" pane_pid target_path parent child executable starttime
    local -a queue children candidates candidate_starttimes
    pane_pid="$(tmux display-message -p -t "$target_session":0 '#{pane_pid}' 2>/dev/null || true)"
    [[ "$pane_pid" =~ ^[0-9]+$ ]] || return 1
    target_path="$(readlink -f -- "$target_executable" 2>/dev/null || true)"
    [ -n "$target_path" ] || return 1
    queue=("$pane_pid")
    candidates=()
    candidate_starttimes=()
    while [ "${#queue[@]}" -gt 0 ]; do
        parent="${queue[0]}"
        queue=("${queue[@]:1}")
        mapfile -t children < <(pgrep -P "$parent" 2>/dev/null || true)
        for child in "${children[@]}"; do
            [[ "$child" =~ ^[0-9]+$ ]] || continue
            queue+=("$child")
            executable=""
            if [ -r "/proc/$child/cmdline" ]; then
                IFS= read -r -d '' executable <"/proc/$child/cmdline" || true
                executable="$(readlink -f -- "$executable" 2>/dev/null || true)"
            fi
            [ "$executable" = "$target_path" ] || continue
            starttime="$(process_starttime "$child" 2>/dev/null || true)"
            [ -n "$starttime" ] || continue
            candidates+=("$child")
            candidate_starttimes+=("$starttime")
        done
    done
    OWNED_PANE_PID="$pane_pid"
    OWNED_PROCESS_EXE="$target_path"
    OWNED_PROCESS_CANDIDATES="${candidates[*]:-}"
    [ "${#candidates[@]}" -eq 1 ] || {
        [ "${#candidates[@]}" -eq 0 ] && return 1
        return 2
    }
    OWNED_PROCESS_PID="${candidates[0]}"
    OWNED_PROCESS_STARTTIME="${candidate_starttimes[0]}"
}

wait_for_owned_process() { # tmux-session executable [timeout]
    local target_session="$1" target_executable="$2" timeout="${3:-10}" deadline status
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        if discover_owned_process "$target_session" "$target_executable"; then
            return 0
        else
            status=$?
        fi
        [ "$status" -ne 2 ] \
            || fail "user.$SCENARIO: tmux pane $target_session has multiple matching process descendants: $OWNED_PROCESS_CANDIDATES"
        sleep 0.05
    done
    fail "user.$SCENARIO: tmux pane $target_session has no matching process descendant"
}

record_owned_process_identity() { # artifact signal
    python3 - "$1" "$session" "$OWNED_PANE_PID" "$OWNED_PROCESS_PID" "$OWNED_PROCESS_STARTTIME" "$OWNED_PROCESS_EXE" "$2" <<'PY'
from pathlib import Path
import json, sys
Path(sys.argv[1]).write_text(json.dumps({
    "tmuxSession": sys.argv[2],
    "panePid": int(sys.argv[3]),
    "rpiPid": int(sys.argv[4]),
    "rpiStarttime": int(sys.argv[5]),
    "executable": sys.argv[6],
    "signal": sys.argv[7],
}, indent=2) + "\n", encoding="utf-8")
PY
}

signal_tui_process() { # signal artifact
    local signal="$1" artifact="$2" selected_pid selected_starttime selected_pane selected_executable
    wait_for_owned_process "$session" "$RPI_BIN" 10
    selected_pid="$OWNED_PROCESS_PID"
    selected_starttime="$OWNED_PROCESS_STARTTIME"
    selected_pane="$OWNED_PANE_PID"
    selected_executable="$OWNED_PROCESS_EXE"
    if ! discover_owned_process "$session" "$RPI_BIN"; then
        fail "user.$SCENARIO: owned TUI process changed before $signal"
    fi
    [ "$OWNED_PROCESS_PID" = "$selected_pid" ] \
        && [ "$OWNED_PROCESS_STARTTIME" = "$selected_starttime" ] \
        && [ "$OWNED_PANE_PID" = "$selected_pane" ] \
        && [ "$OWNED_PROCESS_EXE" = "$selected_executable" ] \
        || fail "user.$SCENARIO: owned TUI process identity changed before $signal"
    record_owned_process_identity "$artifact" "$signal"
    SIGNALED_RPI_PID="$selected_pid"
    kill -s "$signal" "$selected_pid"
}

wait_for_tui_exit() { # marker exit-status-file signaled-pid [timeout] [evidence-name]
    local marker="$1" exit_status_file="$2" signaled_pid="$3" timeout="${4:-30}" output="${5:-tui-exit.txt}"
    local deadline capture exit_status=""
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        capture="$(tmux capture-pane -p -S -3000 -t "$session":0 2>/dev/null || true)"
        printf '%s\n' "$capture" >"$evidence/$output"
        [ ! -e "/proc/$signaled_pid" ] || { sleep 0.1; continue; }
        if [ -s "$exit_status_file" ]; then
            exit_status="$(<"$exit_status_file")"
        fi
        if [ "$exit_status" = "0" ]; then
            case "$capture" in
                *"$marker"*) return 0 ;;
            esac
        fi
        sleep 0.1
    done
    fail "user.$SCENARIO: TUI exit did not prove pid=$signaled_pid status=0 marker=${marker@Q}"
}

verify_process_tree_isolation() { # evidence-dir
    local output_dir="$1" sleeper session_a session_b pane_a pane_b pid_a pid_b deadline
    sleeper="$(readlink -f -- "$(command -v sleep)")"
    session_a="$(unique_tmux_name "$SCENARIO-pid-a")"
    session_b="$(unique_tmux_name "$SCENARIO-pid-b")"
    register_tmux_session "$session_a"
    register_tmux_session "$session_b"
    tmux new-session -d -s "$session_a" "bash -lc \"'$sleeper' 60; printf done\"" \
        \; set-option -t "$session_a" remain-on-exit on
    tmux new-session -d -s "$session_b" "bash -lc \"'$sleeper' 60; printf done\"" \
        \; set-option -t "$session_b" remain-on-exit on
    wait_for_owned_process "$session_a" "$sleeper" 10
    pane_a="$OWNED_PANE_PID"
    pid_a="$OWNED_PROCESS_PID"
    wait_for_owned_process "$session_b" "$sleeper" 10
    pane_b="$OWNED_PANE_PID"
    pid_b="$OWNED_PROCESS_PID"
    [ "$pane_a" != "$pane_b" ] && [ "$pid_a" != "$pid_b" ] \
        || fail "user.$SCENARIO: process-tree isolation fixture identities collided"
    kill -TERM "$pid_a"
    deadline=$((SECONDS + 10))
    while [ -e "/proc/$pid_a" ] && ((SECONDS < deadline)); do sleep 0.05; done
    [ ! -e "/proc/$pid_a" ] || fail "user.$SCENARIO: first process-tree fixture did not exit"
    kill -0 "$pid_b" 2>/dev/null \
        || fail "user.$SCENARIO: signaling the first pane crossed into the second pane"
    discover_owned_process "$session_b" "$sleeper" \
        && [ "$OWNED_PROCESS_PID" = "$pid_b" ] \
        || fail "user.$SCENARIO: second pane lost its exact process descendant"
    python3 - "$output_dir/process-tree-isolation.json" "$pane_a" "$pid_a" "$pane_b" "$pid_b" "$sleeper" <<'PY'
from pathlib import Path
import json, sys
Path(sys.argv[1]).write_text(json.dumps({
    "first": {"panePid": int(sys.argv[2]), "processPid": int(sys.argv[3])},
    "second": {"panePid": int(sys.argv[4]), "processPid": int(sys.argv[5])},
    "executable": sys.argv[6],
    "firstExited": True,
    "secondRemainedAlive": True,
}, indent=2) + "\n", encoding="utf-8")
PY
    kill -TERM "$pid_b" 2>/dev/null || true
    tmux kill-session -t "$session_a" 2>/dev/null || true
    tmux kill-session -t "$session_b" 2>/dev/null || true
}
wait_for_composer_draft() { # sentinel [timeout] [evidence-name]
    local sentinel="$1" timeout="${2:-15}" output="${3:-composer-state.txt}" deadline capture
    deadline=$((SECONDS + timeout))
    tmux send-keys -t "$session":0 -l "$sentinel"
    while ((SECONDS < deadline)); do
        capture="$(tmux capture-pane -p -t "$session":0 2>/dev/null || true)"
        printf '%s\n' "$capture" >"$evidence/$output"
        case "$capture" in
            *"$sentinel"*) return 0 ;;
        esac
        sleep 0.1
    done
    fail "user.$SCENARIO: composer did not accept ${sentinel@Q} (see $evidence/$output)"
}


digest_of() { # text -> user_mock_server.py user_digest value
    printf '%s' "$1" | sha256sum | cut -c1-12
}

wait_for_mock_digest() { # prompt [timeout]
    local prompt="$1" timeout="${2:-20}" digest deadline
    digest="$(digest_of "$prompt")"
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        if grep -F -- "user_digest=$digest" "$evidence/mock-server.log" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    fail "user.$SCENARIO: child prompt ${prompt@Q} never reached the provider (see $evidence/mock-server.log)"
}

panel_to_top() {
    local count
    for ((count = 0; count < 16; count++)); do
        tmux send-keys -t "$session":0 PageUp
    done
    sleep 0.2
}

panel_wait_for() { # needle [timeout] [evidence-name]
    local needle="$1" timeout="${2:-30}" output="${3:-panel-live.txt}"
    local deadline page capture
    deadline=$((SECONDS + timeout))
    panel_to_top
    while ((SECONDS < deadline)); do
        for ((page = 0; page < 16; page++)); do
            capture="$(tmux capture-pane -p -t "$session":0 2>/dev/null || true)"
            printf '%s\n' "$capture" >"$evidence/$output"
            case "$capture" in
                *"$needle"*) return 0 ;;
            esac
            tmux send-keys -t "$session":0 PageDown
            sleep 0.15
        done
        panel_to_top
        sleep 0.35
    done
    fail "user.$SCENARIO: Todo child panel did not show ${needle@Q} (see $evidence/$output)"
}

open_child_subpage() { # [timeout] [evidence-name]
    local timeout="${1:-25}" output="${2:-child-page.txt}" deadline capture
    deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        capture="$(tmux capture-pane -p -t "$session":0 2>/dev/null || true)"
        printf '%s\n' "$capture" >"$evidence/$output"
        case "$capture" in
            *'Todo · Child sessions'*) return 0 ;;
        esac
        tmux send-keys -t "$session":0 Tab
        sleep 0.25
    done
    fail "user.$SCENARIO: unified Todo overlay did not open the Child sessions subpage (see $evidence/$output)"
}

start_mock() { # root -> sets MOCK_PORT
    local root="$1" port_file="$root/mock-port.txt" deadline
    python3 "$E2E_DIR/lib/user_mock_server.py" --scenario sessions --port-file "$port_file" \
        >"$evidence/mock-server.log" 2>&1 &
    register_pid "$!"
    deadline=$((SECONDS + 15))
    while [ ! -s "$port_file" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -s "$port_file" ] || fail "user.$SCENARIO: mock server did not write its port file"
    MOCK_PORT="$(cat "$port_file")"
}

live_todo_child_routing() {
    require_rpi
    require_cmd tmux
    require_cmd python3

    local scenario="todo-child-routing-tui" root evidence session session_root marker release_marker
    local prompt follow_up later_message parent_file child_file child_id capture
    root="$WORK_ROOT/$scenario"
    evidence="$EVIDENCE_ROOT/$scenario"
    rm -rf "$root"
    mkdir -p "$root/workspace/.rpi" "$evidence"
    cat >"$root/workspace/.rpi/settings.json" <<'EOF'
{"defaultProvider":"cliproxy","defaultModel":"gpt-5.6-sol","defaultProjectTrust":"always","selector":{"autoMode":"auto"},"orchestration":{"tasks":true,"todo":true,"maxConcurrency":4,"maxRecursionDepth":2}}
EOF
    session_root="$root/sessions"
    marker="$root/workspace/todo-child-live-marker.txt"
    release_marker="$root/workspace/todo-child-live-release.txt"
    prompt="Implement the acceptance edit by creating todo-child-live-marker.txt in the current workspace with exactly TUI_TODO_CHILD_EDIT_6F21 plus one trailing newline; then use one supervised foreground shell process that waits until todo-child-live-release.txt exists before replying CHILD_ROUTE_RELEASED_6F21."
    follow_up="Reply with exactly MAIN_FOLLOWUP_OK_6F21 and no other text."
    later_message="Reply with exactly SAME_CHILD_OK_6F21 and no other text."
    mkdir -p "$session_root"

    session="$(unique_tmux_name "$scenario")"
    register_tmux_session "$session"
    tmux new-session -d -s "$session" -x 150 -y 46 -c "$root/workspace" \
        "bash -lc \"TERM=xterm-256color '$RPI_BIN' --approve -C '$root/workspace' --session-dir '$session_root' --session-id '$PARENT_SESSION_ID'; status=\\\$?; printf '%s\\\\n' \\\"\\\$status\\\" >'$evidence/rpi.exit'; printf '===TUI-DONE-todo-child-live===\\\\n'\""
    wait_for 'Model' 45 boot-live.txt
    tmux send-keys -t "$session":0 Escape

    parent_file="$session_root/$PARENT_SESSION_ID.jsonl"
    [ -f "$parent_file" ] || fail "user.$scenario: Model-ready TUI has no parent session journal"

    python3 - "$parent_file" "$evidence/boot-live.txt" "$evidence/baseline.json" <<'PY'
from pathlib import Path
import json, re, sys
rows = [json.loads(line) for line in Path(sys.argv[1]).read_text(encoding="utf-8").splitlines() if line.strip()]
screen = Path(sys.argv[2]).read_text(encoding="utf-8", errors="replace")
model = [row for row in rows if row.get("type") == "model_change"]
match = re.search(r"Model\s+([^\s]+/[^\s]+)", screen)
identity = f"{model[-1].get('provider')}/{model[-1].get('modelId')}" if model else (match.group(1) if match else "")
if "/" not in identity:
    raise SystemExit("startup did not expose a provider/model identity")
Path(sys.argv[3]).write_text(json.dumps({
    "messages": sum(row.get("type") == "message" for row in rows),
    "assistantMessages": sum(row.get("type") == "message" and row.get("message", {}).get("role") == "assistant" for row in rows),
    "todos": sum(row.get("type") == "todo_snapshot" for row in rows),
    "models": len(model),
    "providerModelIdentity": identity,
}, indent=2) + "\n", encoding="utf-8")
PY
    send_cmd "$prompt"
    if ! wait_for 'Routed to Todo · ' 45 route-live.txt; then
        fail "user.$scenario: application-loaded selector.autoMode did not route the CodeTask; no config/auth path was inspected or modified"
    fi
    child_id="$(sed -nE 's/.*Routed to Todo · ([[:xdigit:]]{8}-[[:xdigit:]]{4}-[[:xdigit:]]{4}-[[:xdigit:]]{4}-[[:xdigit:]]{12}).*/\1/p' "$evidence/route-live.txt" | tail -n 1)"
    [[ "$child_id" =~ ^[[:xdigit:]]{8}-[[:xdigit:]]{4}-[[:xdigit:]]{4}-[[:xdigit:]]{4}-[[:xdigit:]]{12}$ ]] \
        || fail "user.$scenario: typed route did not expose a child UUID"

    send_cmd '/jobs'
    wait_for 'Jobs' 20 post-route-jobs.txt
    capture="$(cat "$evidence/post-route-jobs.txt")"
    tmux send-keys -t "$session":0 Escape
    printf '%s\n' "$capture" >"$evidence/post-route-parent.txt"
    grep -F 'No orchestration jobs.' "$evidence/post-route-parent.txt" >/dev/null \
        || fail "user.$scenario: typed /jobs panel exposed a parent orchestration job after routing"
    python3 - "$parent_file" "$evidence/baseline.json" "$prompt" "$evidence/post-route-parent.txt" "$evidence/post-route.json" <<'PY'
from pathlib import Path
import json, sys
rows = [json.loads(line) for line in Path(sys.argv[1]).read_text(encoding="utf-8").splitlines() if line.strip()]
baseline = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8"))
prompt = sys.argv[3]
screen = Path(sys.argv[4]).read_text(encoding="utf-8", errors="replace")
messages = sum(row.get("type") == "message" for row in rows)
assistants = sum(row.get("type") == "message" and row.get("message", {}).get("role") == "assistant" for row in rows)
todos = sum(row.get("type") == "todo_snapshot" for row in rows)
models = [row for row in rows if row.get("type") == "model_change"]
jobs = 0 if "No orchestration jobs." in screen else 1
if messages != baseline["messages"] or assistants != baseline["assistantMessages"]:
    raise SystemExit("route-only boundary admitted a parent model turn")
if todos != baseline["todos"]:
    raise SystemExit("route-only boundary mutated the parent Todo DAG")
if models and f"{models[-1].get('provider')}/{models[-1].get('modelId')}" != baseline["providerModelIdentity"]:
    raise SystemExit("route-only boundary changed parent model identity")
if jobs != 0:
    raise SystemExit("route-only boundary created an observable parent task job")
if prompt in json.dumps(rows, ensure_ascii=False):
    raise SystemExit("route-only prompt persisted in the parent transcript")
Path(sys.argv[5]).write_text(json.dumps({
    "parentModelTurnCountForRoute": assistants - baseline["assistantMessages"],
    "parentTaskJobCount": jobs,
    "parentMessageRecordDelta": messages - baseline["messages"],
}, indent=2) + "\n", encoding="utf-8")
PY
    send_cmd '/todo list'
    open_child_subpage 30 live-panel.txt
    panel_wait_for 'Auto-routed Todo' 30 live-label.txt
    panel_wait_for "Parent: $PARENT_SESSION_ID" 30 live-parent.txt
    panel_wait_for 'Status: running' 60 live-running.txt
    tmux capture-pane -p -t "$session":0 >"$evidence/todo-child-running.txt"
    python3 - "$evidence/todo-child-running.txt" "$evidence/todo-child-running.svg" <<'PY'
from pathlib import Path
import html, sys
lines = Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace").splitlines()
width = max((len(line) for line in lines), default=1) * 9 + 32
height = max(len(lines), 1) * 18 + 32
body = "".join(f'<text x="16" y="{28 + index * 18}">{html.escape(line)}</text>' for index, line in enumerate(lines))
svg = f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}"><rect width="100%" height="100%" fill="#111827"/><g fill="#e5e7eb" font-family="monospace" font-size="14">{body}</g></svg>\n'
Path(sys.argv[2]).write_text(svg, encoding="utf-8")
PY
    tmux send-keys -t "$session":0 Escape
    deadline=$((SECONDS + 60))
    while [ ! -f "$marker" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -f "$marker" ] || fail "user.$scenario: child route reached running without the shared-cwd marker"
    [ "$(cat "$marker")" = 'TUI_TODO_CHILD_EDIT_6F21' ] || fail "user.$scenario: shared-cwd marker content mismatch"
    assert_tmux_composer_editable "$session" "$evidence/main-composer-editable-while-child-running.txt" 'MAIN_DRAFT_WHILE_CHILD_RUNNING_6F21'

    send_cmd "$follow_up"
    wait_for 'MAIN_FOLLOWUP_OK_6F21' 120 main-follow-up.txt
    deadline=$((SECONDS + 60))
    while ((SECONDS < deadline)); do
        capture="$(tmux capture-pane -p -t "$session":0 2>/dev/null || true)"
        [[ "$capture" != *'Working'* && "$capture" != *'⟦esc⟧'* ]] && break
        sleep 0.2
    done
    [[ "$capture" != *'Working'* && "$capture" != *'⟦esc⟧'* ]] \
        || fail "user.$scenario: Main follow-up marker appeared before the parent turn settled"
    tmux capture-pane -p -t "$session":0 >"$evidence/main-follow-up-settled-while-child-running.txt"
    send_cmd '/todo list'
    open_child_subpage 30 after-follow-up-panel.txt
    panel_wait_for 'Status: running' 45 after-follow-up-running.txt
    tmux capture-pane -p -t "$session":0 >"$evidence/todo-child-running-after-main.txt"
    python3 - "$evidence/todo-child-running-after-main.txt" "$evidence/todo-child-running-after-main.svg" <<'PY'
from pathlib import Path
import html, sys
lines = Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace").splitlines()
width = max((len(line) for line in lines), default=1) * 9 + 32
height = max(len(lines), 1) * 18 + 32
body = "".join(f'<text x="16" y="{28 + index * 18}">{html.escape(line)}</text>' for index, line in enumerate(lines))
svg = f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}"><rect width="100%" height="100%" fill="#111827"/><g fill="#e5e7eb" font-family="monospace" font-size="14">{body}</g></svg>\n'
Path(sys.argv[2]).write_text(svg, encoding="utf-8")
PY
    python3 - "$evidence/main-follow-up-settled-while-child-running.txt" "$evidence/todo-child-running-after-main.txt" <<'PY'
from pathlib import Path
import sys
main = Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace")
child = Path(sys.argv[2]).read_text(encoding="utf-8", errors="replace")
if "MAIN_FOLLOWUP_OK_6F21" not in main:
    raise SystemExit("settled Main capture omitted the follow-up result")
if "Status: running" not in child:
    raise SystemExit("post-follow-up child capture omitted Running status")
if "Working" in main or "⟦esc⟧" in main:
    raise SystemExit("Main capture was still busy after follow-up")
PY
    tmux send-keys -t "$session":0 q

    send_cmd '/new'
    wait_for 'Started a new session' 30 parent-new-live.txt
    send_cmd '/todo list'
    open_child_subpage 30 new-parent-panel-live.txt
    panel_wait_for 'No Todo children' 30 new-parent-empty-live.txt
    tmux send-keys -t "$session":0 q
    send_cmd "/resume $PARENT_SESSION_ID"
    wait_for 'Resumed ' 30 parent-resume-live.txt
    send_cmd '/todo list'
    open_child_subpage 30 resumed-panel-live.txt
    panel_wait_for "$child_id" 30 resumed-child-live.txt
    tmux send-keys -t "$session":0 q

    printf 'release\n' >"$release_marker"
    send_cmd "/todo send $child_id $later_message"
    wait_for "sent to Todo child $child_id" 30 same-child-send.txt
    send_cmd '/todo list'
    open_child_subpage 30 same-child-panel.txt
    panel_wait_for 'SAME_CHILD_OK_6F21' 180 same-child-complete.txt
    tmux send-keys -t "$session":0 q

    child_file="$session_root/children/$PARENT_SESSION_ID/$child_id.jsonl"
    python3 - "$parent_file" "$child_file" \
        "$evidence/baseline.json" "$evidence/post-route.json" "$prompt" "$follow_up" "$later_message" "$child_id" \
        "$evidence/route-live.txt" "$evidence/tui-evidence.json" <<'PY'
from pathlib import Path
import json, sys
parent_path, child_path, baseline_path, route_boundary_path, routed_prompt, follow_up, later_message, child_id, route_capture, output = sys.argv[1:]
parent = [json.loads(line) for line in Path(parent_path).read_text(encoding="utf-8").splitlines() if line.strip()]
child = [json.loads(line) for line in Path(child_path).read_text(encoding="utf-8").splitlines() if line.strip()]
route_boundary = json.loads(Path(route_boundary_path).read_text(encoding="utf-8"))
baseline = json.loads(Path(baseline_path).read_text(encoding="utf-8"))
parent_text = json.dumps(parent, ensure_ascii=False)
child_text = json.dumps(child, ensure_ascii=False)
model = [row for row in parent if row.get("type") == "model_change"]
todos = [row for row in parent if row.get("type") == "todo_snapshot"]
route_events = Path(route_capture).read_text(encoding="utf-8", errors="replace").count("Routed to Todo ·")
receipt_records = [row["data"]["receipt"] for row in child if row.get("customType") == "pi.todo_child.receipt"]
receipt_ids = [receipt["id"] for receipt in receipt_records if receipt.get("id")]
unique_receipts = {receipt_id for receipt_id in receipt_ids}
if route_events != 1: raise SystemExit(f"typed route count is {route_events}")
if routed_prompt in parent_text: raise SystemExit("routed prompt polluted parent transcript")
if follow_up not in parent_text or "MAIN_FOLLOWUP_OK_6F21" not in parent_text: raise SystemExit("Main follow-up incomplete")
if "MAIN_FOLLOWUP_OK_6F21" in child_text: raise SystemExit("Main follow-up polluted child transcript")
if len(todos) != baseline["todos"]: raise SystemExit("parent Todo state changed")
if model and f"{model[-1].get('provider')}/{model[-1].get('modelId')}" != baseline["providerModelIdentity"]: raise SystemExit("parent model identity changed")
if later_message not in child_text or "SAME_CHILD_OK_6F21" not in child_text: raise SystemExit("same-child later send did not settle")
if len(unique_receipts) != 2: raise SystemExit(f"expected route + later receipt, found {len(unique_receipts)}")
receipt_groups = {receipt_id: [receipt for receipt in receipt_records if receipt.get("id") == receipt_id] for receipt_id in unique_receipts}
route_receipt_id, route_records = next(((receipt_id, records) for receipt_id, records in receipt_groups.items() if any(receipt.get("summary") == "CHILD_ROUTE_RELEASED_6F21" for receipt in records)), (None, None))
if route_receipt_id is None: raise SystemExit("typed route terminal receipt missing")
if [receipt.get("status") for receipt in route_records] != ["queued", "running", "completed"]: raise SystemExit(f"typed route lifecycle is not exactly queued/running/completed: {route_records}")
if route_records[-1].get("summary") != "CHILD_ROUTE_RELEASED_6F21": raise SystemExit("typed route terminal summary mismatch")
if any(receipt.get("childSessionId") != child_id for receipt in route_records): raise SystemExit("typed route receipt child identity mismatch")
evidence = {
    "surface": "tui", "parentSessionId": parent[0]["id"], "childSessionId": child_id,
    "routeReceiptId": route_receipt_id, "routeReceiptCount": route_events,
    "routeReceiptStatuses": [receipt.get("status") for receipt in route_records],
    "routeTerminalDeliveryCount": sum(receipt.get("status") in {"completed", "failed"} for receipt in route_records),
    "parentRoutePromptPresent": False, "parentModelTurnCountForRoute": route_boundary["parentModelTurnCountForRoute"],
    "parentTaskJobCount": route_boundary["parentTaskJobCount"], "parentTodoSnapshotCount": len(todos),
    "parentModelChangeCount": len(model), "providerModelIdentity": baseline["providerModelIdentity"],
    "mainFollowUpCompletedWhileChildRunning": True, "mainComposerEditableWhileChildRunning": True,
    "sharedCwdEdit": True, "sameChildSend": True, "displaySwitchSurvived": True,
    "shutdownFailurePropagated": False,
    "screenshots": ["todo-child-running.svg", "todo-child-running-after-main.svg"],
    "paneEvidence": ["main-composer-editable-while-child-running.txt", "main-follow-up-settled-while-child-running.txt", "todo-child-running-after-main.txt"],
    "pageErrors": [], "consoleErrors": [],
}
Path(output).write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
PY
    send_cmd "/todo send $child_id Keep this receipt active for the shutdown contract: start one supervised foreground shell process that waits until todo-child-shutdown-release.txt exists before replying SHUTDOWN_RELEASED_6F21."
    wait_for "sent to Todo child $child_id" 30 shutdown-work-accepted.txt
    send_cmd '/todo list'
    open_child_subpage 30 shutdown-work-panel.txt
    panel_wait_for 'Status: running' 60 shutdown-work-running.txt
    tmux send-keys -t "$session":0 q
    python3 "$E2E_DIR/live/verify.py" mutate-running-receipt "$child_file" >"$evidence/shutdown-receipt-id.txt"
    send_cmd '/quit'
    deadline=$((SECONDS + 60))
    while [ ! -f "$evidence/rpi.exit" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -f "$evidence/rpi.exit" ] || fail "user.$scenario: TUI did not exit after the shutdown failure"
    [ "$(cat "$evidence/rpi.exit")" -ne 0 ] \
        || fail "user.$scenario: Todo child shutdown failure returned a successful process status"
    python3 - "$evidence/tui-evidence.json" <<'PY'
from pathlib import Path
import json, sys
path = Path(sys.argv[1]); value = json.loads(path.read_text(encoding="utf-8"))
value["shutdownFailurePropagated"] = True
value["shutdownFailureContext"] = "Todo child shutdown returned a nonzero process status after the immutable receipt conflict"
path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
PY
    printf 'user.%s passed\nevidence=%s\n' "$scenario" "$evidence"
}

main() {
    require_rpi
    require_cmd tmux
    require_cmd python3
    require_cmd sha256sum

    local root evidence session port child_id restart_session
    prepare_roots
    scenario_workspace "$SCENARIO" >/dev/null
    root="$WORK_ROOT/$SCENARIO"
    evidence="$EVIDENCE_ROOT/$SCENARIO"
    mkdir -p "$root/home/.rpi"
    verify_process_tree_isolation "$evidence"

    cat >"$root/home/.rpi/settings.json" <<'EOF'
{
  "orchestration": {
    "process": true,
    "tasks": true,
    "todo": true,
    "maxConcurrency": 4,
    "maxRecursionDepth": 2
  },
  "selector": {
    "autoSelectThreshold": 0
  }
}
EOF
    mkdir -p "$root/home/.rpi/agents"
    cat >"$root/home/.rpi/agents/writer.md" <<'EOF'
---
name: writer
description: Deterministic restart worker
tools:
  - read
---
You are the writer agent for durable TUI restart lifecycle verification.
EOF

    start_mock "$root"
    port="$MOCK_PORT"
    cat >"$root/home/.rpi/models.json" <<EOF
{
  "providers": {
    "user-todo-child": {
      "baseUrl": "http://127.0.0.1:$port",
      "api": "openai-completions",
      "models": [
        { "id": "mock", "name": "Todo Child Mock", "contextWindow": 32768, "maxTokens": 2048 }
      ]
    }
  }
}
EOF

    session="$(unique_tmux_name "$SCENARIO")"
    register_tmux_session "$session"
    tmux new-session -d -s "$session" -x 150 -y 46 -c "$root/workspace" \
        "bash -lc \"env HOME='$root/home' USERPROFILE='$root/home' RPI_HOME='$root/home/.rpi' RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 TERM=xterm-256color '$RPI_BIN' --offline --model user-todo-child/mock --api-key e2e-loopback --session-id '$PARENT_SESSION_ID'; status=\\\$?; printf '%s\\\\n' \\\"\\\$status\\\" >'$evidence/rpi-first.exit'; printf '===TUI-DONE-todo-child-first===\\\\n'\"" \
        \; set-option -t "$session" remain-on-exit on

    wait_for 'user-todo-child/mock' 30 boot-first.txt
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    send_cmd "/todo new $CHILD_LABEL"
    wait_for 'created Todo child ' 30 create.txt
    child_id="$(sed -nE 's/.*created Todo child ([[:xdigit:]]{8}-[[:xdigit:]]{4}-[[:xdigit:]]{4}-[[:xdigit:]]{4}-[[:xdigit:]]{12}).*/\1/p' "$evidence/create.txt" | tail -n 1)"
    [[ "$child_id" =~ ^[[:xdigit:]]{8}-[[:xdigit:]]{4}-[[:xdigit:]]{4}-[[:xdigit:]]{4}-[[:xdigit:]]{12}$ ]] \
        || fail "user.$SCENARIO: create response did not expose a Todo child UUID (see $evidence/create.txt)"

    send_cmd "/todo select $child_id"
    wait_for "selected Todo child $child_id" 20 select.txt
    send_cmd '/todo list'
    open_child_subpage 25 panel-open.txt
    panel_wait_for "Label: $CHILD_LABEL" 25 initial-detail-label.txt
    panel_wait_for "Parent: $PARENT_SESSION_ID" 25 initial-detail-parent.txt
    panel_wait_for 'Tasks' 25 initial-detail-tasks.txt
    panel_wait_for 'Receipts' 25 initial-detail-receipts.txt
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    send_cmd "/todo send $child_id $SLOW_MESSAGE"
    wait_for "sent to Todo child $child_id" 20 send-before-switch.txt
    wait_for_mock_digest "$SLOW_MESSAGE" 20
    send_cmd '/new'
    wait_for 'Started a new session' 20 parent-new.txt

    send_cmd '/todo list'
    open_child_subpage 20 new-parent-panel.txt
    panel_wait_for 'No Todo children' 20 new-parent-empty.txt
    tmux send-keys -t "$session":0 Escape
    sleep 0.3

    send_cmd "/resume $PARENT_SESSION_ID"
    wait_for 'Resumed ' 25 parent-resume.txt
    send_cmd '/todo list'
    open_child_subpage 25 resumed-panel.txt
    panel_wait_for "Label: $CHILD_LABEL" 25 resumed-label.txt
    panel_wait_for 'sessions-slow-a-1/' 35 child-continued.txt
    panel_wait_for 'Status: idle' 25 child-idle-after-switch.txt

    tmux send-keys -t "$session":0 a
    wait_for 'Add task [Content]' 15 add-task-input.txt
    tmux send-keys -t "$session":0 -l "$TASK_CONTENT"
    tmux send-keys -t "$session":0 Enter
    panel_wait_for "$TASK_CONTENT" 30 add-task-content.txt
    panel_wait_for "sessions-reply: $TASK_CONTENT" 30 add-task-complete.txt

    tmux send-keys -t "$session":0 i
    wait_for 'Child IRC [Recipient]' 15 irc-input.txt
    tmux send-keys -t "$session":0 -l 'Main'
    tmux send-keys -t "$session":0 Tab
    tmux send-keys -t "$session":0 -l "$IRC_BODY"
    tmux send-keys -t "$session":0 Enter
    panel_wait_for "Main → Main · $IRC_BODY" 25 irc-detail.txt

    tmux send-keys -t "$session":0 p
    panel_wait_for 'Status: paused' 25 paused.txt

    tmux send-keys -t "$session":0 s
    wait_for 'Send message' 15 paused-send-input.txt
    tmux send-keys -t "$session":0 -l "$PAUSED_MESSAGE"
    tmux send-keys -t "$session":0 Enter
    panel_wait_for 'queued · message' 25 paused-receipt-queued.txt
    tmux send-keys -t "$session":0 u
    panel_wait_for "sessions-reply: $PAUSED_MESSAGE" 30 resumed-message-complete.txt
    panel_wait_for 'Status: idle' 25 idle-before-restart.txt

    tmux send-keys -t "$session":0 Escape
    sleep 0.3
    tmux capture-pane -p -S -3000 -t "$session":0 >"$evidence/first-run.txt" 2>/dev/null || true
    send_cmd '/quit'
    if ! tmux_wait_for "$session" 15 '===TUI-DONE-todo-child-first===' >"$evidence/quit-first.txt"; then
        fail "user.$SCENARIO: first /quit did not produce exit marker (see $evidence/quit-first.txt)"
    fi
    tmux kill-session -t "$session" 2>/dev/null || true

    restart_session="$(unique_tmux_name "$SCENARIO-restart")"
    session="$restart_session"
    register_tmux_session "$session"
    tmux new-session -d -s "$session" -x 150 -y 46 -c "$root/workspace" \
        "bash -lc \"env HOME='$root/home' USERPROFILE='$root/home' RPI_HOME='$root/home/.rpi' RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 TERM=xterm-256color '$RPI_BIN' --offline --model user-todo-child/mock --api-key e2e-loopback --resume '$PARENT_SESSION_ID'; status=\\\$?; printf '%s\\\\n' \\\"\\\$status\\\" >'$evidence/rpi-restart.exit'; printf '===TUI-DONE-todo-child-restart===\\\\n'\"" \
        \; set-option -t "$session" remain-on-exit on

    wait_for 'user-todo-child/mock' 30 boot-restart.txt
    tmux send-keys -t "$session":0 Escape
    wait_for_composer_draft 'TODO-SESSION-BOOT-READY' 15 restart-boot-composer.txt
    tmux send-keys -t "$session":0 C-u
    send_cmd '/todo list'
    open_child_subpage 25 restart-panel.txt
    panel_wait_for "Label: $CHILD_LABEL" 25 restart-label.txt
    panel_wait_for "$TASK_CONTENT" 25 restart-task.txt
    panel_wait_for "sessions-reply: $TASK_CONTENT" 25 restart-task-summary.txt
    panel_wait_for "$IRC_BODY" 25 restart-irc.txt
    panel_wait_for "sessions-reply: $PAUSED_MESSAGE" 25 restart-receipt.txt
    panel_wait_for 'Status: idle' 25 restart-idle.txt

    tmux send-keys -t "$session":0 s
    wait_for 'Send message' 15 restart-send-input.txt
    tmux send-keys -t "$session":0 -l "$RESTART_MESSAGE"
    tmux send-keys -t "$session":0 Enter
    panel_wait_for "sessions-reply: $RESTART_MESSAGE" 30 restart-send-complete.txt
    tmux send-keys -t "$session":0 x
    panel_wait_for 'Status: stopped' 25 restart-stopped.txt
    panel_wait_for "$CHILD_LABEL · stopped" 25 restart-list-stopped.txt

    tmux send-keys -t "$session":0 Escape
    sleep 0.3
    tmux send-keys -t "$session":0 Escape
    wait_for_composer_draft 'TODO-SESSION-RECOVERED' 15 restart-composer.txt
    tmux send-keys -t "$session":0 -l 'X'
    wait_for 'TODO-SESSION-RECOVEREDX' 15 restart-composer-appended.txt
    tmux send-keys -t "$session":0 C-u
    tmux capture-pane -p -S -3000 -t "$session":0 >"$evidence/restart-run.txt" 2>/dev/null || true
    send_cmd 'restart-held task spawn'
    local restart_job_id restart_sidecar restart_deadline
    restart_deadline=$((SECONDS + 45))
    restart_job_id=""
    restart_sidecar=""
    while [ "$SECONDS" -lt "$restart_deadline" ]; do
        restart_sidecar="$(python3 - "$root/home" <<'PY'
from pathlib import Path
import json, sys
for path in Path(sys.argv[1]).rglob("orchestration-state.json"):
    try:
        state = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        continue
    for job in state.get("jobs", []):
        if job.get("agentId") == "RestartWorker" and job.get("status") in {"queued", "running"}:
            print(f"{path}\t{job.get('id', '')}")
            raise SystemExit(0)
PY
)"
        if [ -n "$restart_sidecar" ]; then
            restart_job_id="${restart_sidecar#*$'\t'}"
            restart_sidecar="${restart_sidecar%%$'\t'*}"
            break
        fi
        sleep 0.1
    done
    [ -n "$restart_job_id" ] || fail "user.$SCENARIO: Task launch did not persist a queued/running RestartWorker"
    tmux capture-pane -p -S -3000 -t "$session":0 >"$evidence/restart-task-jobs-before.txt" 2>/dev/null || true
    python3 - "$restart_sidecar" "$restart_job_id" "$evidence/restart-task-before.json" <<'PY'
from pathlib import Path
import json, sys
state = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
matches = [job for job in state.get("jobs", []) if job.get("id") == sys.argv[2]]
if len(matches) != 1:
    raise SystemExit(f"expected one persisted queued/running job {sys.argv[2]!r}, got {matches!r}")
job = matches[0]
if job.get("status") not in {"queued", "running"} or job.get("agentId") != "RestartWorker" or job.get("parentId") != "Main":
    raise SystemExit(f"persisted unsettled identity mismatch: {job!r}")
tool_call_id = "call-restart-held-task"
parent_path = Path(state["parentSessionPath"])
rows = [json.loads(line) for line in parent_path.read_text(encoding="utf-8").splitlines() if line.strip()]
def objects(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from objects(child)
    elif isinstance(value, list):
        for child in value:
            yield from objects(child)
all_objects = [item for row in rows for item in objects(row)]
calls = [item for item in all_objects if item.get("id") == tool_call_id and item.get("name") == "task"]
results = [item for item in all_objects if item.get("toolCallId") == tool_call_id]
if len(calls) != 1 or len(results) > 1:
    raise SystemExit(f"pre-restart Task tool identity mismatch: calls={len(calls)} results={len(results)}")
if len(results) == 1:
    receipt = results[0]
    receipt_details = receipt.get("details", [])
    if not receipt_details or receipt_details[0].get("status") not in {"queued", "running"}:
        raise SystemExit(f"pre-restart Task receipt is not queued/running: {receipt!r}")
Path(sys.argv[3]).write_text(json.dumps({
    "sessionId": state.get("parentSessionId"),
    "groupId": state.get("groupId"),
    "toolCallId": tool_call_id,
    "jobId": job.get("id"),
    "agentId": job.get("agentId"),
    "parentId": job.get("parentId"),
    "status": job.get("status"),
}, indent=2) + "\n", encoding="utf-8")
PY
    signal_tui_process TERM "$evidence/restart-signal-identity.json"
    wait_for_tui_exit '===TUI-DONE-todo-child-restart===' "$evidence/rpi-restart.exit" "$SIGNALED_RPI_PID" 30 restart-signal-exit.txt
    curl -s -X POST "http://127.0.0.1:$MOCK_PORT/__release-session" -d '{}' >/dev/null 2>&1 || true
    tmux kill-session -t "$session" 2>/dev/null || true

    restart_session="$(unique_tmux_name "$SCENARIO-task-restart")"
    session="$restart_session"
    register_tmux_session "$session"
    tmux new-session -d -s "$session" -x 150 -y 46 -c "$root/workspace" \
        "bash -lc \"env HOME='$root/home' USERPROFILE='$root/home' RPI_HOME='$root/home/.rpi' RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 TERM=xterm-256color '$RPI_BIN' --offline --model user-todo-child/mock --api-key e2e-loopback --resume '$PARENT_SESSION_ID'; status=\\\$?; printf '%s\\\\n' \\\"\\\$status\\\" >'$evidence/rpi-task-restart.exit'; printf '===TUI-DONE-task-restart===\\\\n'\"" \
        \; set-option -t "$session" remain-on-exit on
    wait_for 'user-todo-child/mock' 30 restart-task-boot.txt
    tmux send-keys -t "$session":0 Escape
    send_cmd '/jobs'
    wait_for 'completed' 60 restart-task-jobs-after.txt
    python3 - "$restart_sidecar" "$evidence/restart-task-before.json" "$evidence/restart-task-jobs-after.txt" "$evidence/restart-task-after.json" <<'PY'
from pathlib import Path
import json, sys
state = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
before = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8"))
screen = Path(sys.argv[3]).read_text(encoding="utf-8", errors="replace")
matches = [job for job in state.get("jobs", []) if job.get("id") == before["jobId"]]
tool_call_id = before["toolCallId"]
parent_path = Path(state["parentSessionPath"])
rows = [json.loads(line) for line in parent_path.read_text(encoding="utf-8").splitlines() if line.strip()]
def objects(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from objects(child)
    elif isinstance(value, list):
        for child in value:
            yield from objects(child)
all_objects = [item for row in rows for item in objects(row)]
calls = [item for item in all_objects if item.get("id") == tool_call_id and item.get("name") == "task"]
results = [item for item in all_objects if item.get("toolCallId") == tool_call_id]
if len(calls) != 1 or len(results) != 1:
    raise SystemExit(f"recovered Task tool identity duplicated: calls={len(calls)} results={len(results)}")
if len(matches) != 1:
    raise SystemExit(f"recovered job identity duplicated or missing: {matches!r}")
job = matches[0]
for field in ("groupId", "parentSessionId"):
    expected = before["groupId" if field == "groupId" else "sessionId"]
    if state.get(field) != expected:
        raise SystemExit(f"{field} changed: {state.get(field)!r} != {expected!r}")
if job.get("agentId") != before["agentId"] or job.get("parentId") != before["parentId"] or job.get("status") != "completed":
    raise SystemExit(f"recovered job mismatch: {job!r}")
if "orchestration shut down" in screen:
    raise SystemExit("TUI restart projected orchestration shut down")
if screen.count(before["jobId"][:11]) != 1:
    raise SystemExit("TUI /jobs did not show exactly one recovered job identity")
output = (job.get("result") or {}).get("output", "")
if output.count("restart-held-subagent identity survived exactly-once") != 1:
    raise SystemExit(f"recovered child output was not exactly once: {output!r}")
Path(sys.argv[4]).write_text(json.dumps({
    "sessionId": state.get("parentSessionId"),
    "groupId": state.get("groupId"),
    "jobId": job.get("id"),
    "agentId": job.get("agentId"),
    "parentId": job.get("parentId"),
    "status": job.get("status"),
    "output": output,
}, indent=2) + "\n", encoding="utf-8")
PY
    tmux send-keys -t "$session":0 Escape
    send_cmd '/quit'
    if ! tmux_wait_for "$session" 15 '===TUI-DONE-task-restart===' >"$evidence/restart-task-quit.txt"; then
        fail "user.$SCENARIO: task-restart /quit did not produce exit marker (see $evidence/restart-task-quit.txt)"
    fi
    tmux kill-session -t "$session" 2>/dev/null || true
    cat >"$evidence/assertions.txt" <<EOF
create=$child_id
select=explicit child id
initial_detail=parent label tasks receipts
parent_cutover=new parent isolated; original child continued
add_task=revision-bound detail form settled
irc=child Main mailbox delivery projected
lifecycle=pause queued send resume idle stop
restart=manifest detail state recovered and accepted new work
process_tree_isolation=two concurrent tmux pane trees retained distinct executable descendants
signal_identity=restart-signal-identity.json records owned panePid and rpiPid before TERM
task_restart=SIGTERM checkpoint preserved session group job agent parent identity and exactly-once output
EOF
    printf 'user.%s passed\nassertions=create,select,detail,send,add-task,irc,pause-resume,parent-switch,cleanup,restart-recovery,process-tree-isolation,owned-signal,task-restart,stop\nevidence=%s\n' \
        "$SCENARIO" "$evidence"
}

case "${1:-run}" in
    --live|live) live_todo_child_routing ;;
    --process-tree-test) prepare_roots; evidence="$EVIDENCE_ROOT/$SCENARIO-process-tree"; mkdir -p "$evidence"; verify_process_tree_isolation "$evidence"; printf 'user.%s process-tree isolation passed\nevidence=%s\n' "$SCENARIO" "$evidence" ;;
    run|all) main ;;
    list|--list|--dry-run) printf '%s\n' 'todo-child-session - deterministic mock lane' 'todo-child-routing-tui - application-loaded public-model routing lane' 'todo-child-session --process-tree-test - deterministic owned-pane descendant isolation' ;;
    *) fail "usage: $0 [run|--live|--process-tree-test|list]" ;;
esac
