#!/usr/bin/env bash
set -euo pipefail
E2E_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
REPO_ROOT="$(CDPATH= cd -- "$E2E_DIR/.." && pwd -P)"
E2E_RUN_ID="${E2E_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
EVIDENCE_ROOT="${EVIDENCE_ROOT:-${TMPDIR:-/tmp}/rpi-e2e-evidence/$E2E_RUN_ID}"
WORK_ROOT="${WORK_ROOT:-${TMPDIR:-/tmp}/rpi-e2e-work/$E2E_RUN_ID}"
RPI_BIN="${RPI_BIN:-$REPO_ROOT/target/release-dist/rpi}"
case "$RPI_BIN" in /*) ;; *) RPI_BIN="$REPO_ROOT/$RPI_BIN" ;; esac
E2E_INITIAL_CLEANUP_PATH="${E2E_CLEANUP_PATHS:-$WORK_ROOT}"
E2E_PIDS=(); E2E_PID_TOKENS=(); E2E_TMUX_SESSIONS=(); E2E_CLEANUP_PATHS=(); E2E_CLEANED=0
log() { printf '[e2e] %s\n' "$*"; }
fail() { printf '[e2e] ERROR: %s\n' "$*" >&2; exit 1; }
require_cmd() { command -v "$1" >/dev/null 2>&1 || fail "missing prerequisite command: $1"; }
require_rpi() { [ -f "$RPI_BIN" ] && [ -x "$RPI_BIN" ] || fail "executable rpi binary not found: $RPI_BIN (build target/release-dist/rpi or set RPI_BIN)"; }
_e2e_canon_path() {
    local p="$1" rest="" part base comp
    case "$p" in
        /*) ;;
        *) p="$PWD/$p" ;;
    esac
    while [ "$p" != "/" ] && [ "${p%/}" != "$p" ]; do p="${p%/}"; done
    while [ "$p" != "/" ] && [ ! -e "$p" ]; do
        part="${p##*/}"
        rest="/$part$rest"
        p="${p%/*}"
        [ -n "$p" ] || p="/"
    done
    base="$(CDPATH= cd -P -- "$p" 2>/dev/null && pwd -P)" || return 1
    [ -n "$base" ] || base="/"
    local -a stack=()
    IFS='/' read -ra _parts <<<"$base" || true
    for comp in "${_parts[@]}"; do [ -n "$comp" ] && stack+=("$comp"); done
    IFS='/' read -ra _parts <<<"${rest#/}" || true
    for comp in "${_parts[@]}"; do
        case "$comp" in
            ""|".") ;;
            "..") [ "${#stack[@]}" -gt 0 ] && unset "stack[$((${#stack[@]}-1))]" ;;
            *) stack+=("$comp") ;;
        esac
    done
    local out=""
    for comp in ${stack[@]+"${stack[@]}"}; do out="$out/$comp"; done
    printf '%s\n' "${out:-/}"
}
_e2e_cleanup_path() {
    local canon repo evidence
    [ -n "$1" ] || return 1
    canon="$(_e2e_canon_path "$1")" || return 1
    repo="$(_e2e_canon_path "$REPO_ROOT")" || return 1
    evidence="$(_e2e_canon_path "$EVIDENCE_ROOT")" || return 1
    case "$canon" in
        /|"$repo"|"$repo"/*|"$evidence") return 1 ;;
    esac
    case "$repo" in "$canon"/*) return 1 ;; esac
    case "$evidence" in "$canon"/*) return 1 ;; esac
    printf '%s\n' "$canon"
}
guard_e2e_roots() {
    local r canon cand
    for r in REPO_ROOT EVIDENCE_ROOT WORK_ROOT; do
        [ -n "${!r}" ] || fail "e2e root $r is empty"
        canon="$(_e2e_canon_path "${!r}")" || fail "e2e root $r cannot be normalized"
        printf -v "$r" '%s' "$canon"
    done
    for cand in EVIDENCE_ROOT WORK_ROOT; do
        case "${!cand}" in
            "$REPO_ROOT"|"$REPO_ROOT"/*) fail "e2e $cand (${!cand}) must not be the repo root or inside it ($REPO_ROOT)" ;;
        esac
    done
}
prepare_roots() { guard_e2e_roots; register_cleanup_path "$E2E_INITIAL_CLEANUP_PATH"; mkdir -p "$EVIDENCE_ROOT" "$WORK_ROOT"; }
scenario_workspace() { local name="$1" root="$WORK_ROOT/$1"; rm -rf "$root"; mkdir -p "$root/home/.rpi" "$root/home/.pi/agent" "$root/workspace" "$EVIDENCE_ROOT/$name"; printf '{}\n' > "$root/home/.rpi/settings.json"; printf '%s\n' "$root"; }
register_pid() { local token; token="$(pid_start_token "$1")"; [ -n "$token" ] || fail "cannot prove process identity for pid $1"; E2E_PIDS+=("$1"); E2E_PID_TOKENS+=("$token"); }
_e2e_unregister_pid() { local p i; local -a keep=() tokens=(); for i in "${!E2E_PIDS[@]}"; do p="${E2E_PIDS[$i]}"; if [ "$p" != "$1" ]; then keep+=("$p"); tokens+=("${E2E_PID_TOKENS[$i]}"); fi; done; E2E_PIDS=("${keep[@]}"); E2E_PID_TOKENS=("${tokens[@]}"); }
_e2e_proc_state() { local stat; stat="$(cat "/proc/$1/stat" 2>/dev/null || true)"; if [ -n "$stat" ]; then stat="${stat##*) }"; printf '%s\n' "${stat%% *}"; else ps -o stat= -p "$1" 2>/dev/null | cut -c1 || true; fi; return 0; }
# Strong identity token for a live pid: /proc starttime (stat field 22 =
# post-pid/comm positional 20). FAILS CLOSED (empty) without /proc — a weak
# identity never matches, so pidfile holders signal nothing.
pid_start_token() { local stat; stat="$(cat "/proc/$1/stat" 2>/dev/null || true)"; if [ -n "$stat" ]; then set -- ${stat##*) }; printf '%s\n' "${20}"; fi; return 0; }
registered_pid_token() { local i; for i in "${!E2E_PIDS[@]}"; do [ "${E2E_PIDS[$i]}" = "$1" ] && { printf '%s\n' "${E2E_PID_TOKENS[$i]}"; return 0; }; done; return 1; }
terminate_registered_pid() { local pid="$1" pgid="${2:-0}" tries="${3:-10}" token now; token="$(registered_pid_token "$pid" || true)"; now="$(pid_start_token "$pid")"; if [ -n "$token" ] && [ -n "$now" ] && [ "$token" = "$now" ]; then terminate_and_reap "$pid" "$pgid" "$tries"; else _e2e_unregister_pid "$pid"; fi; }
# Bounded terminate-and-reap: TERM (direct PID and, when a live PGID is
# supplied, its whole group), poll 0.2s x tries, KILL, then reap ONLY a PID
# known exited (kill -0 fails) or zombie — never an unconditional wait on a
# live or D-state process. Unregisters the PID from E2E_PIDS after the attempt.
terminate_and_reap() { # <pid> [pgid] [tries=10]
    local pid="$1" pgid="${2:-0}" tries="${3:-10}" n=0 state
    # pgid>0 = GROUP-OWNED call: signal ONLY the group (TERM, bounded poll,
    # KILL). The numeric pid is never directly TERM/KILLed afterwards — it
    # may already be reaped or reused by the time the group phase ends — so
    # only wait/reap it once it is known exited or a zombie. pgid=0 =
    # DIRECT-ONLY call on a pid the caller still owns.
    if [ "$pgid" -gt 0 ] 2>/dev/null; then
        if kill -0 -- "-$pgid" 2>/dev/null; then
            kill -TERM -- "-$pgid" 2>/dev/null || true
            n=0
            while [ "$n" -lt "$tries" ] && kill -0 -- "-$pgid" 2>/dev/null; do sleep 0.2; n=$((n + 1)); done
            kill -0 -- "-$pgid" 2>/dev/null && kill -KILL -- "-$pgid" 2>/dev/null || true
        fi
        if [ "$pid" -gt 0 ] 2>/dev/null; then
            n=0
            while [ "$n" -lt "$tries" ]; do
                if ! kill -0 "$pid" 2>/dev/null; then wait "$pid" 2>/dev/null || true; break; fi
                state="$(_e2e_proc_state "$pid")"
                if [ "$state" = "Z" ]; then wait "$pid" 2>/dev/null || true; break; fi
                sleep 0.2
                n=$((n + 1))
            done
        fi
    elif [ "$pid" -gt 0 ] 2>/dev/null; then
        if kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null || true
            n=0
            while [ "$n" -lt "$tries" ] && kill -0 "$pid" 2>/dev/null; do
                state="$(_e2e_proc_state "$pid" || true)"
                if [ "$state" = "Z" ]; then break; fi
                sleep 0.2
                n=$((n + 1))
            done
            if kill -0 "$pid" 2>/dev/null \
                && [ "$(_e2e_proc_state "$pid" || true)" != "Z" ]; then
                kill -KILL "$pid" 2>/dev/null || true
            fi
        fi
        n=0
        while [ "$n" -lt "$tries" ]; do
            if ! kill -0 "$pid" 2>/dev/null; then wait "$pid" 2>/dev/null || true; break; fi
            state="$(_e2e_proc_state "$pid")"
            if [ "$state" = "Z" ]; then wait "$pid" 2>/dev/null || true; break; fi
            sleep 0.2
            n=$((n + 1))
        done
    fi
    _e2e_unregister_pid "$pid"
}
register_tmux_session() { E2E_TMUX_SESSIONS+=("$1"); }
register_cleanup_path() { local canon; canon="$(_e2e_cleanup_path "$1")" || fail "refusing unsafe e2e cleanup path"; E2E_CLEANUP_PATHS+=("$canon"); }
cleanup_e2e() {
    local pid token now session path canon i
    [ "$E2E_CLEANED" -eq 0 ] || return 0
    E2E_CLEANED=1
    trap - EXIT HUP INT TERM
    for session in "${E2E_TMUX_SESSIONS[@]}"; do
        tmux has-session -t "$session" 2>/dev/null && tmux kill-session -t "$session" 2>/dev/null || true
    done
    local -a registered_pids=("${E2E_PIDS[@]}") registered_tokens=("${E2E_PID_TOKENS[@]}")
    for i in "${!registered_pids[@]}"; do
        pid="${registered_pids[$i]}"; token="${registered_tokens[$i]}"; now="$(pid_start_token "$pid")"
        if [ -n "$now" ] && [ "$now" = "$token" ]; then terminate_and_reap "$pid"; fi
    done
    for path in "${E2E_CLEANUP_PATHS[@]}"; do
        if canon="$(_e2e_cleanup_path "$path")"; then rm -rf -- "$canon"; else printf '[e2e] ERROR: refusing unsafe e2e cleanup path\n' >&2; fi
    done
}
# HUP/INT/TERM are TERMINAL: run the one-shot cleanup, then exit 128+signum
# — never resume the interrupted lane. EXIT stays one-shot and idempotent
# via E2E_CLEANED.
_e2e_on_signal() {
    local signum="$1"
    trap - EXIT HUP INT TERM
    cleanup_e2e
    exit $((128 + signum))
}
trap cleanup_e2e EXIT
trap '_e2e_on_signal 1' HUP
trap '_e2e_on_signal 2' INT
trap '_e2e_on_signal 15' TERM
run_with_timeout() { local seconds="$1"; shift; require_cmd timeout; timeout --foreground --signal=TERM --kill-after=5s "${seconds}s" "$@"; }
isolated_rpi() { local home="$1" cwd="$2"; shift 2; env -i HOME="$home" USERPROFILE="$home" PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" TERM="${TERM:-xterm-256color}" PI_CODING_AGENT_DIR="$home/.pi/agent" RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 PI_FAUX_RESPONSE="${PI_FAUX_RESPONSE:-deterministic-e2e-reply}" "$RPI_BIN" --offline -C "$cwd" "$@"; }
isolated_rpi_timeout() { local seconds="$1" home="$2" cwd="$3"; shift 3; require_cmd timeout; env -i HOME="$home" USERPROFILE="$home" PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" TERM="${TERM:-xterm-256color}" PI_CODING_AGENT_DIR="$home/.pi/agent" RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 PI_FAUX_RESPONSE="${PI_FAUX_RESPONSE:-deterministic-e2e-reply}" timeout --foreground --signal=TERM --kill-after=5s "${seconds}s" "$RPI_BIN" --offline -C "$cwd" "$@"; }
current_triple() { case "$(uname -s):$(uname -m)" in Linux:x86_64|Linux:amd64) printf 'x86_64-unknown-linux-gnu\n' ;; Linux:aarch64|Linux:arm64) printf 'aarch64-unknown-linux-gnu\n' ;; Darwin:x86_64) printf 'x86_64-apple-darwin\n' ;; Darwin:arm64|Darwin:aarch64) printf 'aarch64-apple-darwin\n' ;; *) fail "unsupported release fixture platform: $(uname -s) $(uname -m)" ;; esac; }
platform_labels() { case "$1" in x86_64-unknown-linux-gnu) printf 'linux x86_64\n' ;; aarch64-unknown-linux-gnu) printf 'linux aarch64\n' ;; x86_64-apple-darwin) printf 'macos x86_64\n' ;; aarch64-apple-darwin) printf 'macos aarch64\n' ;; *) fail "unknown target triple: $1" ;; esac; }
unique_tmux_name() { printf 'rpi-e2e-%s-%s-%s\n' "$1" "$$" "${RANDOM:-0}"; }
