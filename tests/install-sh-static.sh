#!/bin/sh
set -eu
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
INSTALLER="$REPO_ROOT/install.sh"
ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT HUP INT TERM
FIXTURES="$ROOT/fixtures"
MOCK_BIN="$ROOT/mock-bin"
mkdir -p "$FIXTURES/archive" "$MOCK_BIN"
cat > "$FIXTURES/archive/rpi" <<'EOF'
#!/bin/sh
printf 'rpi 0.1.0\n'
EOF
chmod 0755 "$FIXTURES/archive/rpi"
printf 'license\n' > "$FIXTURES/archive/LICENSE"
ASSET='rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz'
tar -C "$FIXTURES/archive" -czf "$FIXTURES/$ASSET" .
(cd "$FIXTURES" && sha256sum "$ASSET" > SHA256SUMS)
cat > "$FIXTURES/release.json" <<EOF
{"tag_name":"v0.1.0","assets":[{"browser_download_url":"https://example.test/$ASSET"},{"browser_download_url":"https://example.test/SHA256SUMS"}]}
EOF
cat > "$MOCK_BIN/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
  -s) printf 'Linux\n' ;;
  -m) printf 'x86_64\n' ;;
  *) exit 2 ;;
esac
EOF
cat > "$MOCK_BIN/curl" <<'EOF'
#!/bin/sh
out=''; url=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -H) shift 2 ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
case "$url" in
  */releases/latest) src="$TEST_FIXTURES/release.json" ;;
  */rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz) src="$TEST_FIXTURES/rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz" ;;
  */SHA256SUMS) src="$TEST_FIXTURES/SHA256SUMS" ;;
  *) exit 3 ;;
esac
if [ -n "$out" ]; then cp "$src" "$out"; else cat "$src"; fi
EOF
chmod 0755 "$MOCK_BIN/uname" "$MOCK_BIN/curl"
run_install() {
  install_root="$1"; home_dir="$2"; mkdir -p "$home_dir"
  TEST_FIXTURES="$FIXTURES" HOME="$home_dir" PI_HOME="$install_root" PI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >/dev/null
  [ -L "$install_root/bin/rpi" ]
  [ "$("$install_root/bin/rpi" --version)" = 'rpi 0.1.0' ]
  grep -Fq '"installed_asset": "rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz"' "$install_root/update-state.json"
  grep -Fq '"installed_binary": "rpi-0.1.0-linux-x86_64-sha256-' "$install_root/update-state.json"
}
FRESH="$ROOT/fresh"; run_install "$FRESH" "$ROOT/home-fresh"; [ ! -e "$FRESH/bin/pi" ]
MANAGED="$ROOT/managed"
OLD_DIGEST='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
OLD_BINARY="pi-rs-0.0.9-linux-x86_64-sha256-$OLD_DIGEST"
mkdir -p "$MANAGED/bin" "$MANAGED/downloads"
printf '#!/bin/sh\nprintf "pi 0.0.9\\n"\n' > "$MANAGED/downloads/$OLD_BINARY"
chmod 0755 "$MANAGED/downloads/$OLD_BINARY"
ln -s "../downloads/$OLD_BINARY" "$MANAGED/bin/pi"
cat > "$MANAGED/update-state.json" <<EOF
{"installed_version":"0.0.9","installed_asset":"pi-rs-0.0.9-x86_64-unknown-linux-gnu.tar.gz","installed_sha256":"$OLD_DIGEST","installed_binary":"$OLD_BINARY","checked_at_unix":1}
EOF
run_install "$MANAGED" "$ROOT/home-managed"
[ ! -e "$MANAGED/bin/pi" ] && [ ! -L "$MANAGED/bin/pi" ]
UNMANAGED="$ROOT/unmanaged"; mkdir -p "$UNMANAGED/bin"
printf '#!/bin/sh\nprintf "user pi\\n"\n' > "$UNMANAGED/bin/pi"; chmod 0755 "$UNMANAGED/bin/pi"
run_install "$UNMANAGED" "$ROOT/home-unmanaged"; [ -f "$UNMANAGED/bin/pi" ]
NEW_DIGEST="$(awk '{print $1}' "$FIXTURES/SHA256SUMS")"
NEW_BINARY="rpi-0.1.0-linux-x86_64-sha256-$NEW_DIGEST"
DEST_DIR="$ROOT/dest-directory"; mkdir -p "$DEST_DIR/downloads/$NEW_BINARY" "$ROOT/home-dest-directory"
if TEST_FIXTURES="$FIXTURES" HOME="$ROOT/home-dest-directory" PI_HOME="$DEST_DIR" PI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >"$ROOT/dest-directory.out" 2>&1; then exit 1; fi
grep -Fq 'versioned binary path is not a regular file' "$ROOT/dest-directory.out"
[ -d "$DEST_DIR/downloads/$NEW_BINARY" ]; [ ! -e "$DEST_DIR/bin/rpi" ] && [ ! -L "$DEST_DIR/bin/rpi" ]; [ ! -e "$DEST_DIR/update-state.json" ]
ACTIVE_DIR="$ROOT/active-directory"; mkdir -p "$ACTIVE_DIR/bin" "$ACTIVE_DIR/downloads/attacker" "$ROOT/home-active-directory"
ln -s ../downloads/attacker "$ACTIVE_DIR/bin/rpi"
if TEST_FIXTURES="$FIXTURES" HOME="$ROOT/home-active-directory" PI_HOME="$ACTIVE_DIR" PI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >"$ROOT/active-directory.out" 2>&1; then exit 1; fi
grep -Fq 'is a symlink to a directory; refusing unsafe activation' "$ROOT/active-directory.out"
[ "$(readlink "$ACTIVE_DIR/bin/rpi")" = '../downloads/attacker' ]; [ ! -e "$ACTIVE_DIR/update-state.json" ]
STALE="$ROOT/stale"; mkdir -p "$STALE"; printf '999999999\n' > "$STALE/.install.lock"
run_install "$STALE" "$ROOT/home-stale"; [ ! -e "$STALE/.install.lock" ]
cat > "$MOCK_BIN/sleep" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod 0755 "$MOCK_BIN/sleep"
CONTENDED="$ROOT/contended"; mkdir -p "$CONTENDED"; printf '%s\n' "$$" > "$CONTENDED/.install.lock"
if TEST_FIXTURES="$FIXTURES" HOME="$ROOT/home-contended" PI_HOME="$CONTENDED" PI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >"$ROOT/contended.out" 2>&1; then exit 1; fi
grep -Fq 'timed out after 30s waiting for another rpi install' "$ROOT/contended.out"
[ ! -e "$CONTENDED/bin/rpi" ] && [ ! -L "$CONTENDED/bin/rpi" ]; [ "$("$UNMANAGED/bin/pi")" = 'user pi' ]
printf 'install.sh focused behavior tests passed\n'
