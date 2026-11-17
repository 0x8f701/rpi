#!/bin/sh
set -eu
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
INSTALLER="$REPO_ROOT/install.sh"
POWERSHELL_INSTALLER="$REPO_ROOT/install.ps1"
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
WRONG_FIXTURES="$ROOT/wrong-fixtures"
mkdir -p "$WRONG_FIXTURES/archive"
cat > "$WRONG_FIXTURES/archive/rpi" <<'EOF'
#!/bin/sh
printf 'pi 0.1.0\n'
EOF
chmod 0755 "$WRONG_FIXTURES/archive/rpi"
printf 'license\n' > "$WRONG_FIXTURES/archive/LICENSE"
tar -C "$WRONG_FIXTURES/archive" -czf "$WRONG_FIXTURES/$ASSET" .
(cd "$WRONG_FIXTURES" && sha256sum "$ASSET" > SHA256SUMS)
cat > "$FIXTURES/release.json" <<EOF
{"tag_name":"v0.1.0","assets":[{"browser_download_url":"https://example.test/$ASSET"},{"browser_download_url":"https://example.test/SHA256SUMS"}]}
EOF
cp "$FIXTURES/release.json" "$WRONG_FIXTURES/release.json"
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

NETWORK_MOCK_BIN="$ROOT/network-mock-bin"
mkdir -p "$NETWORK_MOCK_BIN"
cp "$MOCK_BIN/uname" "$NETWORK_MOCK_BIN/uname"
cat > "$NETWORK_MOCK_BIN/curl" <<'EOF'
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
  */rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz)
    count_file="$TEST_STATE/archive-attempts"
    count="$(cat "$count_file" 2>/dev/null || printf 0)"
    count=$((count + 1)); printf '%s\n' "$count" > "$count_file"
    [ "$count" -ge "${TEST_ARCHIVE_SUCCEEDS_AT:-4}" ] || exit 35
    src="$TEST_FIXTURES/rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz"
    ;;
  */SHA256SUMS) src="$TEST_FIXTURES/SHA256SUMS" ;;
  *) exit 3 ;;
esac
if [ -n "$out" ]; then cp "$src" "$out"; else cat "$src"; fi
EOF
cat > "$NETWORK_MOCK_BIN/sleep" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod 0755 "$NETWORK_MOCK_BIN/uname" "$NETWORK_MOCK_BIN/curl" "$NETWORK_MOCK_BIN/sleep"

RETRY_ROOT="$ROOT/retry"; RETRY_STATE="$ROOT/retry-state"; mkdir -p "$RETRY_STATE"

# Legacy environment names are rejected before any network request or install
# root is created, and the error message tells the user the canonical name.
LEGACY_HOME="$ROOT/legacy-home"; mkdir -p "$LEGACY_HOME"
if HOME="$LEGACY_HOME" PI_HOME="$ROOT/legacy-pi-home" PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >"$ROOT/legacy-home.out" 2>&1; then exit 1; fi
grep -Fq 'legacy PI_HOME is no longer supported; set RPI_HOME instead' "$ROOT/legacy-home.out"
[ ! -e "$ROOT/legacy-pi-home" ] && [ ! -L "$ROOT/legacy-pi-home" ]
if HOME="$LEGACY_HOME" PI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >"$ROOT/legacy-url.out" 2>&1; then exit 1; fi
grep -Fq 'legacy PI_UPDATE_BASE_URL is no longer supported; set RPI_UPDATE_BASE_URL instead' "$ROOT/legacy-url.out"

TEST_FIXTURES="$FIXTURES" TEST_STATE="$RETRY_STATE" TEST_ARCHIVE_SUCCEEDS_AT=3 HOME="$ROOT/home-retry" RPI_HOME="$RETRY_ROOT" RPI_UPDATE_BASE_URL='https://example.test/releases' PATH="$NETWORK_MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >"$ROOT/retry.out" 2>&1
[ "$(cat "$RETRY_STATE/archive-attempts")" = 3 ]
[ -L "$RETRY_ROOT/bin/rpi" ]
grep -Fq 'Download attempt failed; retrying (2/3)' "$ROOT/retry.out"
grep -Fq 'Download attempt failed; retrying (3/3)' "$ROOT/retry.out"

FAILED_ROOT="$ROOT/network-failed"; FAILED_STATE="$ROOT/network-failed-state"; mkdir -p "$FAILED_STATE"
if TEST_FIXTURES="$FIXTURES" TEST_STATE="$FAILED_STATE" TEST_ARCHIVE_SUCCEEDS_AT=4 HOME="$ROOT/home-network-failed" RPI_HOME="$FAILED_ROOT" RPI_UPDATE_BASE_URL='https://example.test/releases' PATH="$NETWORK_MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >"$ROOT/network-failed.out" 2>&1; then exit 1; fi
grep -Fq 'download failed: https://example.test/rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz' "$ROOT/network-failed.out"
if grep -Eq 'release_install_lock: (not found|command not found)|command not found.*release_install_lock' "$ROOT/network-failed.out"; then exit 1; fi
[ ! -e "$FAILED_ROOT/.install.lock" ]
[ ! -e "$FAILED_ROOT/bin/rpi" ] && [ ! -L "$FAILED_ROOT/bin/rpi" ]
run_install() {
  install_root="$1"; home_dir="$2"; mkdir -p "$home_dir"
  TEST_FIXTURES="$FIXTURES" HOME="$home_dir" RPI_HOME="$install_root" RPI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >/dev/null
  [ -L "$install_root/bin/rpi" ]
  [ "$("$install_root/bin/rpi" --version)" = 'rpi 0.1.0' ]
  grep -Fq '"installed_asset": "rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz"' "$install_root/update-state.json"
  grep -Fq '"installed_binary": "rpi-0.1.0-linux-x86_64-sha256-' "$install_root/update-state.json"
}
DEFAULT_HOME="$ROOT/home-default"; mkdir -p "$DEFAULT_HOME"
TEST_FIXTURES="$FIXTURES" HOME="$DEFAULT_HOME" RPI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh env -u RPI_HOME sh "$INSTALLER" >/dev/null
[ -L "$DEFAULT_HOME/.rpi/bin/rpi" ]
[ ! -e "$DEFAULT_HOME/.pi-rs/bin/rpi" ] && [ ! -L "$DEFAULT_HOME/.pi-rs/bin/rpi" ]
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
MISMATCH="$ROOT/mismatch"
run_install "$MISMATCH" "$ROOT/home-mismatch"
MISMATCH_TARGET="$(readlink "$MISMATCH/bin/rpi")"
cp "$MISMATCH/update-state.json" "$ROOT/mismatch-state.before"
set -- "$MISMATCH"/downloads/*
MISMATCH_DOWNLOAD_COUNT="$#"
if TEST_FIXTURES="$WRONG_FIXTURES" HOME="$ROOT/home-mismatch" RPI_HOME="$MISMATCH" RPI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >"$ROOT/mismatch.out" 2>&1; then exit 1; fi
grep -Fq "downloaded binary reported unexpected identity/version (expected 'rpi 0.1.0'); existing install left untouched" "$ROOT/mismatch.out"
[ "$(readlink "$MISMATCH/bin/rpi")" = "$MISMATCH_TARGET" ]
[ "$("$MISMATCH/bin/rpi" --version)" = 'rpi 0.1.0' ]
cmp "$ROOT/mismatch-state.before" "$MISMATCH/update-state.json"
set -- "$MISMATCH"/downloads/*
[ "$#" -eq "$MISMATCH_DOWNLOAD_COUNT" ]
NEW_DIGEST="$(awk '{print $1}' "$FIXTURES/SHA256SUMS")"
NEW_BINARY="rpi-0.1.0-linux-x86_64-sha256-$NEW_DIGEST"
DEST_DIR="$ROOT/dest-directory"; mkdir -p "$DEST_DIR/downloads/$NEW_BINARY" "$ROOT/home-dest-directory"
if TEST_FIXTURES="$FIXTURES" HOME="$ROOT/home-dest-directory" RPI_HOME="$DEST_DIR" RPI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >"$ROOT/dest-directory.out" 2>&1; then exit 1; fi
grep -Fq 'versioned binary path is not a regular file' "$ROOT/dest-directory.out"
[ -d "$DEST_DIR/downloads/$NEW_BINARY" ]; [ ! -e "$DEST_DIR/bin/rpi" ] && [ ! -L "$DEST_DIR/bin/rpi" ]; [ ! -e "$DEST_DIR/update-state.json" ]
ACTIVE_DIR="$ROOT/active-directory"; mkdir -p "$ACTIVE_DIR/bin" "$ACTIVE_DIR/downloads/attacker" "$ROOT/home-active-directory"
ln -s ../downloads/attacker "$ACTIVE_DIR/bin/rpi"
if TEST_FIXTURES="$FIXTURES" HOME="$ROOT/home-active-directory" RPI_HOME="$ACTIVE_DIR" RPI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >"$ROOT/active-directory.out" 2>&1; then exit 1; fi
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
if TEST_FIXTURES="$FIXTURES" HOME="$ROOT/home-contended" RPI_HOME="$CONTENDED" RPI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >"$ROOT/contended.out" 2>&1; then exit 1; fi
grep -Fq 'timed out after 30s waiting for another rpi install' "$ROOT/contended.out"
[ ! -e "$CONTENDED/bin/rpi" ] && [ ! -L "$CONTENDED/bin/rpi" ]; [ "$("$UNMANAGED/bin/pi")" = 'user pi' ]

# Installer-created managed state is owner-only regardless of the caller's
# umask (0700 directories, 0600 state file), while installed executables keep
# their 0755 mode. Run under umask 000, the permissive worst case.
mode_of() {
    stat -c '%a' "$1" 2>/dev/null || stat -f '%Lp' "$1"
}
UMASK_ROOT="$ROOT/umask"; mkdir -p "$ROOT/home-umask"
( umask 000
  TEST_FIXTURES="$FIXTURES" HOME="$ROOT/home-umask" RPI_HOME="$UMASK_ROOT" RPI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >/dev/null )
[ -L "$UMASK_ROOT/bin/rpi" ]
[ "$("$UMASK_ROOT/bin/rpi" --version)" = 'rpi 0.1.0' ]
[ "$(mode_of "$UMASK_ROOT")" = '700' ]
[ "$(mode_of "$UMASK_ROOT/downloads")" = '700' ]
[ "$(mode_of "$UMASK_ROOT/bin")" = '700' ]
[ "$(mode_of "$UMASK_ROOT/update-state.json")" = '600' ]
[ "$(mode_of "$UMASK_ROOT/downloads/$NEW_BINARY")" = '755' ]
# The install lock exists only while held; its 0600 mode is asserted below via
# the installer source contract.
grep -Fq 'chmod 0700 "$path" || err "could not secure $label permissions: $path"' "$INSTALLER"
grep -Fq 'printf '\''%s\n'\'' "$$" > "$LOCKFILE" && chmod 0600 "$LOCKFILE"' "$INSTALLER"
grep -Fq 'chmod 0600 "$STATE_FILE" || fail_after_rollback "could not secure rpi update state permissions"' "$INSTALLER"
if command -v pwsh >/dev/null 2>&1; then
  POWERSHELL_INSTALLER="$POWERSHELL_INSTALLER" pwsh -NoProfile -Command '$errors = $null; [System.Management.Automation.Language.Parser]::ParseFile((Resolve-Path $env:POWERSHELL_INSTALLER), [ref]$null, [ref]$errors) | Out-Null; if ($errors.Count -ne 0) { $errors | ForEach-Object { Write-Error $_ }; exit 1 }'
  # install.ps1 fails closed: both entry points exit non-zero with the Windows
  # unavailability notice and leave an isolated HOME/RPI_HOME untouched.
  # PSModuleAnalysisCachePath is pinned so pwsh's own cache cannot dirty the
  # sandbox; every remaining write would come from install.ps1.
  PS_SANDBOX="$ROOT/pwsh-sandbox"; mkdir -p "$PS_SANDBOX/home" "$PS_SANDBOX/rpi"
  for ps_args in '' '-Help'; do
    if HOME="$PS_SANDBOX/home" RPI_HOME="$PS_SANDBOX/rpi" PSModuleAnalysisCachePath=/dev/null pwsh -NoProfile -File "$POWERSHELL_INSTALLER" $ps_args >"$ROOT/pwsh$ps_args.out" 2>&1; then exit 1; fi
    grep -Fq 'Windows builds are not published' "$ROOT/pwsh$ps_args.out"
  done
  [ -z "$(find "$PS_SANDBOX/home" "$PS_SANDBOX/rpi" -mindepth 1 -print)" ]
else
  printf 'SKIP: pwsh not found; install.ps1 execution checks not run\n'
fi
printf 'install.sh focused behavior tests passed\n'
