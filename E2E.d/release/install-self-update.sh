#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
. "$SCRIPT_DIR/../lib/common.sh"
list_scenarios() {
    printf '%s\n' \
        'release.https-required - installer rejects non-HTTPS metadata URLs before any fetch' \
        'release.loopback-http-flag - loopback HTTP requires RPI_ALLOW_LOOPBACK_HTTP=1' \
        'release.empty-effective-url - curl success with empty HTTP status is refused' \
        'release.off-host-redirect - off-host intermediate redirect is refused before contact' \
        'release.install - install.sh downloads/checksums/smokes a local release fixture' \
        'release.identity-mismatch - checksum-valid exit-0 wrong identity preserves install and state' \
        'release.legacy-managed - installer removes only a proven legacy managed pi symlink' \
        'release.legacy-unmanaged - installer preserves an unmanaged pi path' \
        'release.self-update - managed rpi update --self reports current release up to date' \
        'release.lock-timeout - optional 30-second live-owner install lock timeout (RUN_LOCK_TIMEOUT=1)'
}
case "${1:-run}" in list|--list|--dry-run) list_scenarios; exit 0 ;; run) ;; *) fail "usage: $0 [run|list|--dry-run]" ;; esac
require_rpi; require_cmd python3; require_cmd tar; require_cmd timeout; prepare_roots
grep -F -- "--proto '=https'" "$REPO_ROOT/install.sh" >/dev/null \
    || fail "install.sh is missing curl HTTPS protocol pin"
grep -F -- "--proto-redir '=https'" "$REPO_ROOT/install.sh" >/dev/null \
    || fail "install.sh is missing curl HTTPS redirect pin"
grep -F -- '--https-only' "$REPO_ROOT/install.sh" >/dev/null \
    || fail "install.sh is missing wget HTTPS-only pin"
if grep -F '"$STAGED" --version' "$REPO_ROOT/install.sh" >/dev/null; then
    fail "install.sh still executes the staged candidate"
fi
grep -F 'asset_url_for_name' "$REPO_ROOT/install.sh" >/dev/null \
    || fail "install.sh no longer reconstructs asset URLs from the metadata origin"
grep -F 'refusing download with empty effective URL' "$REPO_ROOT/install.sh" >/dev/null \
    || fail "install.sh no longer rejects an empty redirect URL"
grep -F 'refusing download with empty HTTP status' "$REPO_ROOT/install.sh" >/dev/null \
    || fail "install.sh no longer rejects an empty curl HTTP status"
grep -F 'http_redirect_location' "$REPO_ROOT/install.sh" >/dev/null \
    || fail "install.sh no longer validates redirect hops"
if grep -F -- '--max-redirect=5' "$REPO_ROOT/install.sh" >/dev/null; then
    fail "install.sh still lets wget follow GitHub redirects without hop checks"
fi
if grep -F -- '--max-redirs 5' "$REPO_ROOT/install.sh" >/dev/null; then
    fail "install.sh still lets curl follow GitHub redirects without hop checks"
fi
if grep -F -- '%{url_effective}' "$REPO_ROOT/install.sh" >/dev/null; then
    fail "install.sh still trusts curl's final effective URL instead of each hop"
fi
root="$(scenario_workspace release-install)"; evidence="$EVIDENCE_ROOT/release-install"; fixture="$root/fixture"; install_root="$root/install"; version="$(timeout --foreground --kill-after=5s 30s "$RPI_BIN" --version | sed -n 's/^rpi //p')"
[ -n "$version" ] || fail "could not parse rpi version"
https_probe="$root/https-probe"; mkdir -p "$https_probe"
if env -i HOME="$root/home" PATH="${PATH:-/usr/bin:/bin}" RPI_HOME="$https_probe" RPI_UPDATE_BASE_URL="http://example.invalid/releases" SHELL=/bin/sh timeout --foreground --kill-after=2s 5s sh "$REPO_ROOT/install.sh" --version "$version" > "$evidence/https-required.log" 2>&1; then fail "installer accepted a non-HTTPS metadata URL"; fi
grep -F 'refusing non-HTTPS URL' "$evidence/https-required.log" >/dev/null \
    || fail "installer did not refuse a non-HTTPS metadata URL"
if env -i HOME="$root/home" PATH="${PATH:-/usr/bin:/bin}" RPI_HOME="$https_probe" RPI_UPDATE_BASE_URL="http://127.0.0.1:1/releases" SHELL=/bin/sh timeout --foreground --kill-after=2s 5s sh "$REPO_ROOT/install.sh" --version "$version" > "$evidence/loopback-http-flag.log" 2>&1; then fail "installer accepted loopback HTTP without RPI_ALLOW_LOOPBACK_HTTP=1"; fi
grep -F 'RPI_ALLOW_LOOPBACK_HTTP=1' "$evidence/loopback-http-flag.log" >/dev/null \
    || fail "installer did not require RPI_ALLOW_LOOPBACK_HTTP=1 for loopback HTTP"
empty_curl_bin="$root/empty-curl-bin"; mkdir -p "$empty_curl_bin"
cat > "$empty_curl_bin/curl" <<'EOF'
#!/bin/sh
out=''
hdr=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -D|--dump-header) hdr="$2"; shift 2 ;;
    -H) shift 2 ;;
    --proto|--proto-redir|--max-redirs|-w|--write-out) shift 2 ;;
    -*) shift ;;
    *) shift ;;
  esac
done
[ -n "$out" ] && printf 'mock-body\n' > "$out"
[ -n "$hdr" ] && printf 'HTTP/1.1 200 OK\n\n' > "$hdr"
exit 0
EOF
chmod 0755 "$empty_curl_bin/curl"
if env -i HOME="$root/home" PATH="$empty_curl_bin:${PATH:-/usr/bin:/bin}" RPI_HOME="$https_probe" SHELL=/bin/sh timeout --foreground --kill-after=2s 5s sh "$REPO_ROOT/install.sh" --version "$version" > "$evidence/empty-effective-url.log" 2>&1; then fail "installer accepted an empty curl HTTP status"; fi
grep -F 'refusing download with empty HTTP status' "$evidence/empty-effective-url.log" >/dev/null \
    || fail "installer did not refuse an empty curl HTTP status"
offhost_curl_bin="$root/offhost-curl-bin"; mkdir -p "$offhost_curl_bin"
evil_contacted="$root/evil-intermediate-contacted"
cat > "$offhost_curl_bin/curl" <<EOF
#!/bin/sh
follow=0
out=''
hdr=''
write_out=''
url=''
while [ "\$#" -gt 0 ]; do
  case "\$1" in
    -o) out="\$2"; shift 2 ;;
    -D|--dump-header) hdr="\$2"; shift 2 ;;
    -w|--write-out) write_out="\$2"; shift 2 ;;
    -H) shift 2 ;;
    -L|--location) follow=1; shift ;;
    --proto|--proto-redir|--max-redirs) shift 2 ;;
    -*)
      case "\$1" in
        --*) shift ;;
        *L*) follow=1; shift ;;
        *) shift ;;
      esac
      ;;
    *) url="\$1"; shift ;;
  esac
done
case "\$url" in
  *evil.example*)
    printf contacted >> "$evil_contacted"
    [ -n "\$out" ] && printf 'evil-body\\n' > "\$out"
    [ -n "\$hdr" ] && printf 'HTTP/1.1 302 Found\\nLocation: https://api.github.com/repos/0x8f701/rpi/releases/latest\\n\\n' > "\$hdr"
    case "\$write_out" in
      *http_code*) printf '%s' '302' ;;
      *url_effective*) printf '%s' 'https://api.github.com/repos/0x8f701/rpi/releases/latest' ;;
    esac
    exit 0
    ;;
esac
if [ "\$follow" -eq 1 ]; then
    printf contacted >> "$evil_contacted"
    [ -n "\$out" ] && printf 'mock-body\\n' > "\$out"
    [ -n "\$hdr" ] && printf 'HTTP/1.1 200 OK\\n\\n' > "\$hdr"
    case "\$write_out" in
      *http_code*) printf '%s' '200' ;;
      *url_effective*) printf '%s' 'https://api.github.com/repos/0x8f701/rpi/releases/latest' ;;
    esac
    exit 0
fi
[ -n "\$out" ] && printf '' > "\$out"
[ -n "\$hdr" ] && printf 'HTTP/1.1 302 Found\\nLocation: https://evil.example/redirect\\n\\n' > "\$hdr"
case "\$write_out" in
  *http_code*) printf '%s' '302' ;;
  *url_effective*) printf '%s' "\$url" ;;
esac
exit 0
EOF
chmod 0755 "$offhost_curl_bin/curl"
if env -i HOME="$root/home" PATH="$offhost_curl_bin:${PATH:-/usr/bin:/bin}" RPI_HOME="$https_probe" SHELL=/bin/sh timeout --foreground --kill-after=2s 5s sh "$REPO_ROOT/install.sh" --version "$version" > "$evidence/off-host-redirect.log" 2>&1; then fail "installer accepted an off-host intermediate redirect"; fi
grep -F 'release metadata redirected to an unexpected host: https://evil.example/redirect' "$evidence/off-host-redirect.log" >/dev/null \
    || fail "installer did not refuse an off-host intermediate redirect"
[ ! -e "$evil_contacted" ] || fail "installer contacted the off-host intermediate redirect target"
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
env -i HOME="$root/home" PATH="${PATH:-/usr/bin:/bin}" RPI_HOME="$install_root" RPI_UPDATE_BASE_URL="$base" RPI_ALLOW_LOOPBACK_HTTP=1 SHELL=/bin/sh timeout --foreground --kill-after=5s 45s sh "$REPO_ROOT/install.sh" --version "$version" > "$evidence/install.log" 2>&1
[ -x "$install_root/bin/rpi" ] && [ -L "$install_root/bin/rpi" ] || fail "installer did not activate managed rpi"
[ ! -e "$install_root/bin/pi" ] && [ ! -L "$install_root/bin/pi" ] || fail "legacy managed pi was not removed"
timeout --foreground --kill-after=5s 30s "$install_root/bin/rpi" --version > "$evidence/version.log"
env -i HOME="$root/home" PATH="${PATH:-/usr/bin:/bin}" RPI_HOME="$install_root" RPI_UPDATE_BASE_URL="$base" RPI_SKIP_VERSION_CHECK=1 timeout --foreground --kill-after=5s 45s "$install_root/bin/rpi" update --self > "$evidence/self-update.log" 2>&1
grep -F 'already up to date' "$evidence/self-update.log" >/dev/null
unmanaged="$install_root/bin/pi"; printf '#!/bin/sh\nprintf unmanaged\n' > "$unmanaged"; chmod 0755 "$unmanaged"
env -i HOME="$root/home" PATH="${PATH:-/usr/bin:/bin}" RPI_HOME="$install_root" RPI_UPDATE_BASE_URL="$base" RPI_ALLOW_LOOPBACK_HTTP=1 SHELL=/bin/sh timeout --foreground --kill-after=5s 45s sh "$REPO_ROOT/install.sh" --version "$version" > "$evidence/reinstall.log" 2>&1
[ -f "$unmanaged" ] && [ ! -L "$unmanaged" ] || fail "unmanaged pi path was modified"
active_target="$(readlink "$install_root/bin/rpi")"
cp "$install_root/update-state.json" "$root/update-state.before-mismatch.json"
set -- "$install_root"/downloads/*
download_count_before_mismatch="$#"
mkdir -p "$fixture/mismatch"
cat > "$fixture/mismatch/rpi" <<EOF
#!/bin/sh
printf 'pi $version\\n'
EOF
chmod 0755 "$fixture/mismatch/rpi"
cp "$REPO_ROOT/LICENSE" "$fixture/mismatch/LICENSE"
tar -C "$fixture/mismatch" -czf "$fixture/$asset" .
if command -v sha256sum >/dev/null 2>&1; then mismatch_digest="$(sha256sum "$fixture/$asset" | cut -d ' ' -f 1)"; else mismatch_digest="$(shasum -a 256 "$fixture/$asset" | cut -d ' ' -f 1)"; fi
printf '%s  %s\n' "$mismatch_digest" "$asset" > "$fixture/SHA256SUMS"
if env -i HOME="$root/home" PATH="${PATH:-/usr/bin:/bin}" RPI_HOME="$install_root" RPI_UPDATE_BASE_URL="$base" RPI_ALLOW_LOOPBACK_HTTP=1 SHELL=/bin/sh timeout --foreground --kill-after=5s 45s sh "$REPO_ROOT/install.sh" --version "$version" > "$evidence/identity-mismatch.log" 2>&1; then fail "installer accepted an exit-0 binary with the wrong identity"; fi
grep -F "downloaded binary reported unexpected identity/version (expected 'rpi $version'); existing install left untouched" "$evidence/identity-mismatch.log" >/dev/null
[ "$(readlink "$install_root/bin/rpi")" = "$active_target" ] || fail "identity mismatch replaced the active rpi link"
cmp "$root/update-state.before-mismatch.json" "$install_root/update-state.json" || fail "identity mismatch replaced updater state"
set -- "$install_root"/downloads/*
[ "$#" -eq "$download_count_before_mismatch" ] || fail "identity mismatch persisted candidate bytes"
if [ "${RUN_LOCK_TIMEOUT:-0}" = 1 ]; then
    printf '%s\n' "$$" > "$install_root/.install.lock"; start="$(date +%s)"
    if env -i HOME="$root/home" PATH="${PATH:-/usr/bin:/bin}" RPI_HOME="$install_root" RPI_UPDATE_BASE_URL="$base" RPI_ALLOW_LOOPBACK_HTTP=1 SHELL=/bin/sh timeout --foreground --kill-after=5s 40s sh "$REPO_ROOT/install.sh" --version "$version" > "$evidence/lock-timeout.log" 2>&1; then fail "installer unexpectedly acquired a live-owner lock"; fi
    elapsed=$(( $(date +%s) - start )); [ "$elapsed" -ge 30 ] && [ "$elapsed" -le 40 ] || fail "install lock timeout was not bounded: ${elapsed}s"; grep -F 'timed out after 30s' "$evidence/lock-timeout.log" >/dev/null
fi
printf 'release install/self-update passed (9 required scenarios, 1 identity mismatch rejection; optional lock timeout=%s)\nevidence=%s\n' "${RUN_LOCK_TIMEOUT:-0}" "$evidence"
