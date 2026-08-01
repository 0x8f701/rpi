#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
. "$SCRIPT_DIR/../lib/common.sh"
list_scenarios() {
    printf '%s\n' \
        'release.install - install.sh downloads/checksums/smokes a local release fixture' \
        'release.legacy-managed - installer removes only a proven legacy managed pi symlink' \
        'release.legacy-unmanaged - installer preserves an unmanaged pi path' \
        'release.self-update - managed rpi update --self reports current release up to date' \
        'release.lock-timeout - optional 30-second live-owner install lock timeout (RUN_LOCK_TIMEOUT=1)'
}
case "${1:-run}" in list|--list|--dry-run) list_scenarios; exit 0 ;; run) ;; *) fail "usage: $0 [run|list|--dry-run]" ;; esac
require_rpi; require_cmd python3; require_cmd tar; require_cmd timeout; prepare_roots
root="$(scenario_workspace release-install)"; evidence="$EVIDENCE_ROOT/release-install"; fixture="$root/fixture"; install_root="$root/install"; version="$(timeout --foreground --kill-after=5s 30s "$RPI_BIN" --version | sed -n 's/^rpi //p')"
[ -n "$version" ] || fail "could not parse rpi version"
triple="$(current_triple)"; read -r platform_os platform_arch < <(platform_labels "$triple"); asset="rpi-$version-$triple.tar.gz"
mkdir -p "$fixture/staging" "$install_root/bin" "$install_root/downloads"
cp "$RPI_BIN" "$fixture/staging/rpi"; chmod 0755 "$fixture/staging/rpi"; cp "$REPO_ROOT/LICENSE" "$fixture/staging/LICENSE"; tar -C "$fixture/staging" -czf "$fixture/$asset" .
if command -v sha256sum >/dev/null 2>&1; then digest="$(sha256sum "$fixture/$asset" | cut -d ' ' -f 1)"; legacy_digest="$(printf legacy-managed | sha256sum | cut -d ' ' -f 1)"; else digest="$(shasum -a 256 "$fixture/$asset" | cut -d ' ' -f 1)"; legacy_digest="$(printf legacy-managed | shasum -a 256 | cut -d ' ' -f 1)"; fi
printf '%s  %s\n' "$digest" "$asset" > "$fixture/SHA256SUMS"
legacy_binary="pi-rs-$version-$platform_os-$platform_arch-sha256-$legacy_digest"
printf '#!/bin/sh\nexit 0\n' > "$install_root/downloads/$legacy_binary"; chmod 0755 "$install_root/downloads/$legacy_binary"; ln -s "../downloads/$legacy_binary" "$install_root/bin/pi"
printf '{\n  "installed_version": "%s",\n  "installed_asset": "pi-rs-%s-%s.tar.gz",\n  "installed_sha256": "%s",\n  "installed_binary": "%s",\n  "checked_at_unix": 1\n}\n' "$version" "$version" "$triple" "$legacy_digest" "$legacy_binary" > "$install_root/update-state.json"
port_file="$root/port"; python3 "$E2E_DIR/lib/release_fixture_server.py" --root "$fixture" --version "$version" --port-file "$port_file" "$asset" SHA256SUMS > "$evidence/server.log" 2>&1 &
server_pid=$!; register_pid "$server_pid"
for _ in $(seq 1 100); do [ -s "$port_file" ] && break; sleep 0.05; done
[ -s "$port_file" ] || fail "release fixture server failed to start"; base="http://127.0.0.1:$(cat "$port_file")/releases"
env -i HOME="$root/home" PATH="${PATH:-/usr/bin:/bin}" PI_HOME="$install_root" PI_UPDATE_BASE_URL="$base" SHELL=/bin/sh timeout --foreground --kill-after=5s 45s sh "$REPO_ROOT/install.sh" --version "$version" > "$evidence/install.log" 2>&1
[ -x "$install_root/bin/rpi" ] && [ -L "$install_root/bin/rpi" ] || fail "installer did not activate managed rpi"
[ ! -e "$install_root/bin/pi" ] && [ ! -L "$install_root/bin/pi" ] || fail "legacy managed pi was not removed"
timeout --foreground --kill-after=5s 30s "$install_root/bin/rpi" --version > "$evidence/version.log"
env -i HOME="$root/home" PATH="${PATH:-/usr/bin:/bin}" PI_HOME="$install_root" PI_UPDATE_BASE_URL="$base" PI_SKIP_VERSION_CHECK=1 timeout --foreground --kill-after=5s 45s "$install_root/bin/rpi" update --self > "$evidence/self-update.log" 2>&1
grep -F 'already up to date' "$evidence/self-update.log" >/dev/null
unmanaged="$install_root/bin/pi"; printf '#!/bin/sh\nprintf unmanaged\n' > "$unmanaged"; chmod 0755 "$unmanaged"
env -i HOME="$root/home" PATH="${PATH:-/usr/bin:/bin}" PI_HOME="$install_root" PI_UPDATE_BASE_URL="$base" SHELL=/bin/sh timeout --foreground --kill-after=5s 45s sh "$REPO_ROOT/install.sh" --version "$version" > "$evidence/reinstall.log" 2>&1
[ -f "$unmanaged" ] && [ ! -L "$unmanaged" ] || fail "unmanaged pi path was modified"
if [ "${RUN_LOCK_TIMEOUT:-0}" = 1 ]; then
    printf '%s\n' "$$" > "$install_root/.install.lock"; start="$(date +%s)"
    if env -i HOME="$root/home" PATH="${PATH:-/usr/bin:/bin}" PI_HOME="$install_root" PI_UPDATE_BASE_URL="$base" SHELL=/bin/sh timeout --foreground --kill-after=5s 40s sh "$REPO_ROOT/install.sh" --version "$version" > "$evidence/lock-timeout.log" 2>&1; then fail "installer unexpectedly acquired a live-owner lock"; fi
    elapsed=$(( $(date +%s) - start )); [ "$elapsed" -ge 30 ] && [ "$elapsed" -le 40 ] || fail "install lock timeout was not bounded: ${elapsed}s"; grep -F 'timed out after 30s' "$evidence/lock-timeout.log" >/dev/null
fi
printf 'release install/self-update passed\nevidence=%s\n' "$evidence"
