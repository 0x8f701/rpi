#!/usr/bin/env bash
set -euo pipefail
E2E_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
REPO_ROOT="$(CDPATH= cd -- "$E2E_DIR/.." && pwd -P)"
E2E_RUN_ID="${E2E_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
EVIDENCE_ROOT="${EVIDENCE_ROOT:-${TMPDIR:-/tmp}/rpi-e2e-evidence/$E2E_RUN_ID}"
WORK_ROOT="${WORK_ROOT:-${TMPDIR:-/tmp}/rpi-e2e-work/$E2E_RUN_ID}"
RPI_BIN="${RPI_BIN:-$REPO_ROOT/target/release-dist/rpi}"
case "$RPI_BIN" in /*) ;; *) RPI_BIN="$REPO_ROOT/$RPI_BIN" ;; esac
E2E_PIDS=(); E2E_TMUX_SESSIONS=(); E2E_CLEANUP_PATHS=("$WORK_ROOT"); E2E_CLEANED=0
log() { printf '[e2e] %s\n' "$*"; }
fail() { printf '[e2e] ERROR: %s\n' "$*" >&2; exit 1; }
require_cmd() { command -v "$1" >/dev/null 2>&1 || fail "missing prerequisite command: $1"; }
require_rpi() { [ -f "$RPI_BIN" ] && [ -x "$RPI_BIN" ] || fail "executable rpi binary not found: $RPI_BIN (build target/release-dist/rpi or set RPI_BIN)"; }
prepare_roots() { mkdir -p "$EVIDENCE_ROOT" "$WORK_ROOT"; }
scenario_workspace() { local name="$1" root="$WORK_ROOT/$1"; rm -rf "$root"; mkdir -p "$root/home/.pi/agent" "$root/workspace" "$EVIDENCE_ROOT/$name"; printf '{}\n' > "$root/home/.pi/agent/settings.json"; printf '%s\n' "$root"; }
register_pid() { E2E_PIDS+=("$1"); }
register_tmux_session() { E2E_TMUX_SESSIONS+=("$1"); }
register_cleanup_path() { E2E_CLEANUP_PATHS+=("$1"); }
cleanup_e2e() { local pid session path; [ "$E2E_CLEANED" -eq 0 ] || return 0; E2E_CLEANED=1; trap - EXIT HUP INT TERM; for session in "${E2E_TMUX_SESSIONS[@]}"; do tmux has-session -t "$session" 2>/dev/null && tmux kill-session -t "$session" 2>/dev/null || true; done; for pid in "${E2E_PIDS[@]}"; do if kill -0 "$pid" 2>/dev/null; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi; done; for path in "${E2E_CLEANUP_PATHS[@]}"; do rm -rf "$path"; done; }
trap cleanup_e2e EXIT HUP INT TERM
run_with_timeout() { local seconds="$1"; shift; require_cmd timeout; timeout --foreground --signal=TERM --kill-after=5s "${seconds}s" "$@"; }
isolated_rpi() { local home="$1" cwd="$2"; shift 2; env -i HOME="$home" USERPROFILE="$home" PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" TERM="${TERM:-xterm-256color}" PI_CODING_AGENT_DIR="$home/.pi/agent" PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_FAUX_RESPONSE="${PI_FAUX_RESPONSE:-deterministic-e2e-reply}" "$RPI_BIN" --offline -C "$cwd" "$@"; }
isolated_rpi_timeout() { local seconds="$1" home="$2" cwd="$3"; shift 3; require_cmd timeout; env -i HOME="$home" USERPROFILE="$home" PATH="${PATH:-/usr/bin:/bin}" LANG="${LANG:-C.UTF-8}" LC_ALL="${LC_ALL:-C.UTF-8}" TERM="${TERM:-xterm-256color}" PI_CODING_AGENT_DIR="$home/.pi/agent" PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_FAUX_RESPONSE="${PI_FAUX_RESPONSE:-deterministic-e2e-reply}" timeout --foreground --signal=TERM --kill-after=5s "${seconds}s" "$RPI_BIN" --offline -C "$cwd" "$@"; }
current_triple() { case "$(uname -s):$(uname -m)" in Linux:x86_64|Linux:amd64) printf 'x86_64-unknown-linux-gnu\n' ;; Linux:aarch64|Linux:arm64) printf 'aarch64-unknown-linux-gnu\n' ;; Darwin:x86_64) printf 'x86_64-apple-darwin\n' ;; Darwin:arm64|Darwin:aarch64) printf 'aarch64-apple-darwin\n' ;; *) fail "unsupported release fixture platform: $(uname -s) $(uname -m)" ;; esac; }
platform_labels() { case "$1" in x86_64-unknown-linux-gnu) printf 'linux x86_64\n' ;; aarch64-unknown-linux-gnu) printf 'linux aarch64\n' ;; x86_64-apple-darwin) printf 'macos x86_64\n' ;; aarch64-apple-darwin) printf 'macos aarch64\n' ;; *) fail "unknown target triple: $1" ;; esac; }
unique_tmux_name() { printf 'rpi-e2e-%s-%s-%s\n' "$1" "$$" "${RANDOM:-0}"; }
