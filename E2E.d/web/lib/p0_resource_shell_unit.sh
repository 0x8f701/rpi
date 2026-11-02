#!/usr/bin/env bash
# Shell-level unit for p0_resource.sh: source-guard behavior + mock lifecycle.
#
# Sources the REAL p0_resource.sh (and through it lib/common.sh and
# lib/fixture.sh) under isolated runtime temp roots, starts the real
# lib/p0_resource_mock.py via start_local_mock in the PARENT shell, and
# verifies that MOCK_PORT is set, the mock PID is registered in E2E_PIDS and
# alive, and that cleanup_e2e fully reaps it. Never launches rpi --listen or
# Playwright, so it needs no built binary, node, Chromium, or npm.
#
# The parent then spawns a CHILD bash (`bash "$0" --child`) that sources the
# real p0_resource.sh under its own temp roots, starts the real mock, writes
# its PID for the parent, and intentionally exits nonzero WITHOUT any manual
# cleanup. Only the child's EXIT trap (installed by common.sh) may reap the
# mock; the parent polls bounded and asserts kill -0 fails, proving the trap
# cleaned up on the failure path with no process/temp leak.
#
# A second child mode (`bash "$0" --success-child`) covers the NORMAL exit path:
# same sourcing/mock/registration, but the child simply returns 0 without any
# manual cleanup — mirroring the real harness's successful main() return,
# which also relies on shell EXIT cleanup. The parent asserts status 0 and
# that the EXIT trap reaped the mock and removed the registered work root
# there too.
#
# Usage:
#   bash E2E.d/web/lib/p0_resource_shell_unit.sh

set -euo pipefail

HERE="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
SELF="$HERE/$(basename -- "${BASH_SOURCE[0]}")"

# --- child failure-path scenario ------------------------------------------
# Runs in a SEPARATE bash spawned by the parent with --child: sources the
# real p0_resource.sh under its own temp roots (it must never inherit the
# parent's EVIDENCE_ROOT/WORK_ROOT — its EXIT trap removes registered
# cleanup paths), starts the real mock, writes the mock PID and port for
# the parent to poll, then intentionally exits nonzero WITHOUT any manual
# cleanup. The EXIT trap common.sh installs must reap the mock and drop the
# registered work root; the parent asserts both.
child_main() {
    local child_root="$CHILD_ROOT" child_evidence child_work root
    # Evidence must be OUTSIDE the registered work root: register_cleanup_path
    # refuses any cleanup path containing EVIDENCE_ROOT. The work root holds
    # the mock's temp artifacts and is removed by the child's own EXIT trap;
    # the evidence dir sits under $child_root and the parent's unit_cleanup
    # drops the entire UNIT_ROOT tree.
    child_work="$child_root/work"
    child_evidence="$child_root/evidence"
    mkdir -p "$child_work" "$child_evidence"
    export WORK_ROOT="$child_work"
    export EVIDENCE_ROOT="$child_evidence"
    # Sourcing installs `trap cleanup_e2e EXIT HUP INT TERM` in THIS shell,
    # so every PID registered from here on is reaped when we exit nonzero.
    . "$HERE/../p0_resource.sh"
    register_cleanup_path "$child_work"
    root="$child_work/fail-unit"
    mkdir -p "$root"
    start_local_mock "$root" "$child_evidence"
    printf '%s\n' "${MOCK_PORT:-}" >"$child_root/child-port.txt"
    printf '%s\n' "$(cat "$root/mock.pid")" >"$child_root/child-mock.pid"
    # Intentional failure, no manual cleanup: the EXIT trap must reap the
    # mock and remove $child_work while the shell exits nonzero.
    exit "${CHILD_EXIT_CODE:-17}"
}
# --- child normal-success path --------------------------------------------
# Same setup, but exits/returns 0 WITHOUT any manual cleanup, exactly like
# the real harness's successful main() return. The EXIT trap must reap the
# mock and remove the registered work root on the NORMAL exit path too; the
# parent asserts status 0 plus bounded PID/work-root disappearance.
child_success_main() {
    local child_root="$CHILD_ROOT" child_evidence child_work root
    child_work="$child_root/work"
    child_evidence="$child_root/evidence"
    mkdir -p "$child_work" "$child_evidence"
    export WORK_ROOT="$child_work"
    export EVIDENCE_ROOT="$child_evidence"
    . "$HERE/../p0_resource.sh"
    register_cleanup_path "$child_work"
    root="$child_work/success-unit"
    mkdir -p "$root"
    start_local_mock "$root" "$child_evidence"
    printf '%s\n' "${MOCK_PORT:-}" >"$child_root/success-port.txt"
    printf '%s\n' "$(cat "$root/mock.pid")" >"$child_root/success-mock.pid"
    # Normal exit, no manual cleanup: the EXIT trap must reap the mock and
    # remove $child_work while the shell exits 0.
    exit 0
}
if [ "${1:-}" = "--child" ]; then
    child_main
fi
if [ "${1:-}" = "--success-child" ]; then
    child_success_main
fi

UNIT_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/p0-resource-shell-unit.XXXXXX")"
export EVIDENCE_ROOT="$UNIT_ROOT/evidence"
export WORK_ROOT="$UNIT_ROOT/work"
mkdir -p "$EVIDENCE_ROOT" "$WORK_ROOT"

# cleanup_e2e (from common.sh) runs `trap - EXIT HUP INT TERM` at its start,
# which disarms any EXIT handler set before it. unit_cleanup owns the temp
# tree, calls cleanup_e2e (idempotent) to reap registered PIDs, then
# re-installs itself. The trap is installed BEFORE sourcing so even a source
# failure leaves no temp root behind and every success/failure path still
# reaps the mock.
unit_cleanup() {
    rm -rf -- "$UNIT_ROOT"
    if command -v cleanup_e2e >/dev/null 2>&1; then cleanup_e2e; fi
    trap unit_cleanup EXIT HUP INT TERM
}
trap unit_cleanup EXIT HUP INT TERM

. "$HERE/../p0_resource.sh"

# common.sh installed `trap cleanup_e2e EXIT HUP INT TERM`; take over the
# handlers so the temp roots are always dropped (cleanup_e2e is idempotent
# and reaps the mock on success AND failure paths).
trap unit_cleanup EXIT HUP INT TERM

# --- source guard: sourcing defines functions only, runs nothing ----------
[ "$(type -t main)" = "function" ] || {
    printf '[shell-unit] FAIL: main is not a function after sourcing\n' >&2
    exit 1
}
[ "$(type -t start_local_mock)" = "function" ] || {
    printf '[shell-unit] FAIL: start_local_mock is not a function after sourcing\n' >&2
    exit 1
}
[ "${#E2E_PIDS[@]}" -eq 0 ] || {
    printf '[shell-unit] FAIL: sourcing registered processes (guard ran main)\n' >&2
    exit 1
}
[ ! -e "$EVIDENCE_ROOT/web-p0-resource" ] || {
    printf '[shell-unit] FAIL: sourcing created the live-lane evidence dir (guard ran main)\n' >&2
    exit 1
}

# --- mock lifecycle ---------------------------------------------------------
root="$WORK_ROOT/unit"
mkdir -p "$root"
start_local_mock "$root" "$EVIDENCE_ROOT"

[ -n "${MOCK_PORT:-}" ] || {
    printf '[shell-unit] FAIL: MOCK_PORT not set by start_local_mock\n' >&2
    exit 1
}
case "$MOCK_PORT" in
    ''|*[!0-9]*) printf '[shell-unit] FAIL: MOCK_PORT not a numeric port: %s\n' "$MOCK_PORT" >&2; exit 1 ;;
esac
[ "$MOCK_PORT" -gt 0 ] || {
    printf '[shell-unit] FAIL: MOCK_PORT not a positive port: %s\n' "$MOCK_PORT" >&2
    exit 1
}

mock_pid="$(cat "$root/mock.pid")"
case "$mock_pid" in
    ''|*[!0-9]*) printf '[shell-unit] FAIL: mock.pid not a numeric pid: %s\n' "$mock_pid" >&2; exit 1 ;;
esac

found=0
for pid in "${E2E_PIDS[@]}"; do
    [ "$pid" = "$mock_pid" ] && found=1
done
[ "$found" -eq 1 ] || {
    printf '[shell-unit] FAIL: mock pid %s absent from parent E2E_PIDS\n' "$mock_pid" >&2
    exit 1
}
kill -0 "$mock_pid" 2>/dev/null || {
    printf '[shell-unit] FAIL: mock pid %s is not alive\n' "$mock_pid" >&2
    exit 1
}

cleanup_e2e
# cleanup_e2e disarmed the traps (`trap - EXIT HUP INT TERM`); re-arm so the
# EXIT handler still drops the temp tree.
trap unit_cleanup EXIT HUP INT TERM
if kill -0 "$mock_pid" 2>/dev/null; then
    printf '[shell-unit] FAIL: mock pid %s still alive after cleanup_e2e\n' "$mock_pid" >&2
    exit 1
fi
for pid in "${E2E_PIDS[@]}"; do
    if kill -0 "$pid" 2>/dev/null; then
        printf '[shell-unit] FAIL: pid %s still alive after cleanup_e2e\n' "$pid" >&2
        exit 1
    fi
done

# --- child failure path: EXIT trap only, no manual cleanup, no leak --------
child_root="$UNIT_ROOT/child"
mkdir -p "$child_root"
set +e
CHILD_ROOT="$child_root" CHILD_EXIT_CODE=17 bash "$SELF" --child >"$child_root/child.out" 2>&1
child_status=$?
set -e
if [ "$child_status" -ne 17 ]; then
    printf '[shell-unit] FAIL: failure child exited %s, expected 17 (nonzero without manual cleanup)\n' "$child_status" >&2
    printf '[shell-unit] child output:\n' >&2
    cat "$child_root/child.out" >&2 || true
    exit 1
fi
if [ ! -s "$child_root/child-mock.pid" ]; then
    printf '[shell-unit] FAIL: failure child did not write child-mock.pid\n' >&2
    printf '[shell-unit] child output:\n' >&2
    cat "$child_root/child.out" >&2 || true
    exit 1
fi
child_mock_pid="$(cat "$child_root/child-mock.pid")"
case "$child_mock_pid" in
    ''|*[!0-9]*) printf '[shell-unit] FAIL: child mock.pid not a numeric pid: %s\n' "$child_mock_pid" >&2; exit 1 ;;
esac
child_mock_port="$(cat "$child_root/child-port.txt")"
case "$child_mock_port" in
    ''|*[!0-9]*) printf '[shell-unit] FAIL: child mock port not numeric: %s\n' "$child_mock_port" >&2; exit 1 ;;
esac
# The child performed NO manual cleanup, so only its EXIT trap can have
# reaped the mock. Poll bounded (SIGTERM teardown is not instantaneous)
# and assert the PID is gone.
deadline=$((SECONDS + 10))
while kill -0 "$child_mock_pid" 2>/dev/null; do
    [ "$SECONDS" -lt "$deadline" ] || {
        printf '[shell-unit] FAIL: child mock pid %s still alive after failure exit (EXIT trap did not reap)\n' "$child_mock_pid" >&2
        printf '[shell-unit] child output:\n' >&2
        cat "$child_root/child.out" >&2 || true
        exit 1
    }
    sleep 0.1
done
# No temp leak: the child's EXIT trap removed the work root it registered.
[ ! -e "$child_root/work" ] || {
    printf '[shell-unit] FAIL: child work root left behind: %s\n' "$child_root/work" >&2
    exit 1
}

# --- child normal-success path: EXIT trap only, no manual cleanup, no leak --
# The success child returns 0 WITHOUT any manual cleanup (mirroring the real
# harness's successful main() return); only its EXIT trap may reap the mock.
child_root="$UNIT_ROOT/child-success"
mkdir -p "$child_root"
set +e
CHILD_ROOT="$child_root" bash "$SELF" --success-child >"$child_root/success.out" 2>&1
success_status=$?
set -e
if [ "$success_status" -ne 0 ]; then
    printf '[shell-unit] FAIL: success child exited %s, expected 0 (normal return without manual cleanup)\n' "$success_status" >&2
    printf '[shell-unit] child output:\n' >&2
    cat "$child_root/success.out" >&2 || true
    exit 1
fi
if [ ! -s "$child_root/success-mock.pid" ]; then
    printf '[shell-unit] FAIL: success child did not write success-mock.pid\n' >&2
    printf '[shell-unit] child output:\n' >&2
    cat "$child_root/success.out" >&2 || true
    exit 1
fi
success_mock_pid="$(cat "$child_root/success-mock.pid")"
case "$success_mock_pid" in
    ''|*[!0-9]*) printf '[shell-unit] FAIL: success child mock.pid not a numeric pid: %s\n' "$success_mock_pid" >&2; exit 1 ;;
esac
success_mock_port="$(cat "$child_root/success-port.txt")"
case "$success_mock_port" in
    ''|*[!0-9]*) printf '[shell-unit] FAIL: success child mock port not numeric: %s\n' "$success_mock_port" >&2; exit 1 ;;
esac
# The success child performed NO manual cleanup, so only its EXIT trap can
# have reaped the mock after the normal 0 return. Poll bounded (SIGTERM
# teardown is not instantaneous) and assert the PID is gone.
deadline=$((SECONDS + 10))
while kill -0 "$success_mock_pid" 2>/dev/null; do
    [ "$SECONDS" -lt "$deadline" ] || {
        printf '[shell-unit] FAIL: success child mock pid %s still alive after exit 0 (EXIT trap did not reap)\n' "$success_mock_pid" >&2
        printf '[shell-unit] child output:\n' >&2
        cat "$child_root/success.out" >&2 || true
        exit 1
    }
    sleep 0.1
done
# No temp leak: the success child's EXIT trap removed its registered work
# root on the normal exit path.
[ ! -e "$child_root/work" ] || {
    printf '[shell-unit] FAIL: success child work root left behind: %s\n' "$child_root/work" >&2
    exit 1
}

printf 'shell-unit: p0_resource source guard + mock lifecycle + normal/failure EXIT reap: PASSED (mock pid=%s port=%s; normal child exit=%s, reaped pid=%s; failure child exit=%s, reaped pid=%s)\n' "$mock_pid" "$MOCK_PORT" "$success_status" "$success_mock_pid" "$child_status" "$child_mock_pid"
