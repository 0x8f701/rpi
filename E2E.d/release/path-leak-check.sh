#!/usr/bin/env bash
set -euo pipefail
# Scans a built release binary for forbidden builder-path prefixes.
#
# The release gate (release.yml) and local release-dist builds run this after
# building: it reports, per category, how many binary byte runs match a
# forbidden absolute prefix (HOME, workspace, CARGO_HOME, RUSTUP_HOME) and
# fails when any count is nonzero. Matching strings are never printed, so the
# check doubles as a privacy-safe leak gate. rustc embeds remapped source
# paths as UTF-8 strings (file!/panic locations); debug sections are stripped
# by the release-dist profile, and the PowerShell twin additionally covers
# UTF-16LE wide strings for PE binaries.
#
# Usage:
#   E2E.d/release/path-leak-check.sh BINARY KEY=PREFIX [KEY=PREFIX...]
#
# Example:
#   E2E.d/release/path-leak-check.sh target/release-dist/rpi \
#     "HOME=$HOME" "WORKSPACE=$PWD" \
#     "CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}" \
#     "RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}"
list() {
    printf '%s\n' 'release.path-leak - scan a built release binary for forbidden builder-path prefixes (counts per category only)'
}
case "${1:-list}" in
    list|--list|--dry-run) list; exit 0 ;;
esac
[ "$#" -ge 2 ] || { printf 'usage: %s BINARY KEY=PREFIX [KEY=PREFIX...]\n' "$0" >&2; exit 2; }
binary="$1"
shift
[ -f "$binary" ] || { printf 'path-leak-check: binary not found: %s\n' "$binary" >&2; exit 2; }

total=0
violations=0
for spec in "$@"; do
    case "$spec" in
        *=*) ;;
        *) printf 'path-leak-check: expected KEY=PREFIX, got: %s\n' "$spec" >&2; exit 2 ;;
    esac
    key="${spec%%=*}"
    prefix="${spec#*=}"
    [ -n "$key" ] && [ -n "$prefix" ] || {
        printf 'path-leak-check: expected KEY=PREFIX, got: %s\n' "$spec" >&2
        exit 2
    }
    # -a scans the binary as text, -c counts matching lines (an embedded
    # absolute path is one contiguous byte run), -F treats the prefix as a
    # literal string. grep exits 1 on zero matches while still printing 0.
    count="$(grep -a -c -F -e "$prefix" -- "$binary" 2>/dev/null || true)"
    printf '%s: %s\n' "$key" "$count"
    total=$((total + count))
    if [ "$count" -ne 0 ]; then
        violations=$((violations + 1))
    fi
done
if [ "$violations" -ne 0 ]; then
    printf 'path-leak-check: %s forbidden builder-path occurrence(s) in %s (see counts above)\n' "$total" "$binary" >&2
    exit 1
fi
printf 'no builder path leakage detected\n'
