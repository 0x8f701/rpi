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
run_install() {
  install_root="$1"; home_dir="$2"; mkdir -p "$home_dir"
  TEST_FIXTURES="$FIXTURES" HOME="$home_dir" PI_HOME="$install_root" PI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >/dev/null
  [ -L "$install_root/bin/rpi" ]
  [ "$("$install_root/bin/rpi" --version)" = 'rpi 0.1.0' ]
  grep -Fq '"installed_asset": "rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz"' "$install_root/update-state.json"
  grep -Fq '"installed_binary": "rpi-0.1.0-linux-x86_64-sha256-' "$install_root/update-state.json"
}
DEFAULT_HOME="$ROOT/home-default"; mkdir -p "$DEFAULT_HOME"
TEST_FIXTURES="$FIXTURES" HOME="$DEFAULT_HOME" PI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >/dev/null
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
if TEST_FIXTURES="$WRONG_FIXTURES" HOME="$ROOT/home-mismatch" PI_HOME="$MISMATCH" PI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >"$ROOT/mismatch.out" 2>&1; then exit 1; fi
grep -Fq "downloaded binary reported unexpected identity/version (expected 'rpi 0.1.0'); existing install left untouched" "$ROOT/mismatch.out"
[ "$(readlink "$MISMATCH/bin/rpi")" = "$MISMATCH_TARGET" ]
[ "$("$MISMATCH/bin/rpi" --version)" = 'rpi 0.1.0' ]
cmp "$ROOT/mismatch-state.before" "$MISMATCH/update-state.json"
set -- "$MISMATCH"/downloads/*
[ "$#" -eq "$MISMATCH_DOWNLOAD_COUNT" ]
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

# Installer-created managed state is owner-only regardless of the caller's
# umask (0700 directories, 0600 state file), while installed executables keep
# their 0755 mode. Run under umask 000, the permissive worst case.
mode_of() {
    stat -c '%a' "$1" 2>/dev/null || stat -f '%Lp' "$1"
}
UMASK_ROOT="$ROOT/umask"; mkdir -p "$ROOT/home-umask"
( umask 000
  TEST_FIXTURES="$FIXTURES" HOME="$ROOT/home-umask" PI_HOME="$UMASK_ROOT" PI_UPDATE_BASE_URL='https://example.test/releases' PATH="$MOCK_BIN:$PATH" SHELL=/bin/sh sh "$INSTALLER" >/dev/null )
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
fi
grep -Fq 'function Get-CandidateIdentityFailure([string]$Path, [string]$ExpectedVersion)' "$POWERSHELL_INSTALLER"
grep -Fq '$CandidateOutput.Count -ne 1 -or [string]$CandidateOutput[0] -cne "rpi $ExpectedVersion"' "$POWERSHELL_INSTALLER"
PRE_SMOKE_LINE="$(grep -n 'Get-CandidateIdentityFailure $Binary.FullName $ResolvedVersion' "$POWERSHELL_INSTALLER" | cut -d: -f1)"
STAGED_SMOKE_LINE="$(grep -n 'Get-CandidateIdentityFailure $Staged $ResolvedVersion' "$POWERSHELL_INSTALLER" | cut -d: -f1)"
ACTIVATION_LINE="$(grep -n '\[PiInstall.Native\]::MoveFileEx($Staged, $Dest' "$POWERSHELL_INSTALLER" | cut -d: -f1)"
STATE_COMMIT_LINE="$(grep -n 'Move-Item -LiteralPath $StateTmp -Destination $StatePath' "$POWERSHELL_INSTALLER" | cut -d: -f1)"
[ -n "$PRE_SMOKE_LINE" ] && [ -n "$STAGED_SMOKE_LINE" ] && [ -n "$ACTIVATION_LINE" ] && [ -n "$STATE_COMMIT_LINE" ]
[ "$PRE_SMOKE_LINE" -lt "$ACTIVATION_LINE" ]
[ "$STAGED_SMOKE_LINE" -lt "$ACTIVATION_LINE" ]
[ "$PRE_SMOKE_LINE" -lt "$STATE_COMMIT_LINE" ]

# PowerShell installer: GITHUB_TOKEN authenticates only the fixed GitHub API
# endpoint; downloads and custom PI_UPDATE_BASE_URL endpoints never see it.
grep -Fq '#   GITHUB_TOKEN           authenticate the fixed GitHub API endpoint (default:' "$POWERSHELL_INSTALLER"
grep -Fq '$GitHubApiBase = "https://api.github.com/repos/$Repo/releases"' "$POWERSHELL_INSTALLER"
grep -Fq 'if ($env:GITHUB_TOKEN -and $ApiBase -eq $GitHubApiBase)' "$POWERSHELL_INSTALLER"
grep -Fq '$ApiHeaders["Authorization"] = "Bearer $env:GITHUB_TOKEN"' "$POWERSHELL_INSTALLER"
grep -Fq 'Invoke-RestMethod -Uri $ReleaseUrl -Headers $ApiHeaders' "$POWERSHELL_INSTALLER"
grep -Fq 'Invoke-WebRequest -Uri $ArchiveAsset.browser_download_url -Headers $Headers' "$POWERSHELL_INSTALLER"
grep -Fq 'Invoke-WebRequest -Uri $SumsAsset.browser_download_url -Headers $Headers' "$POWERSHELL_INSTALLER"
TOKEN_BASE_LINE="$(grep -Fn '$GitHubApiBase = "https://api.github.com/repos/$Repo/releases"' "$POWERSHELL_INSTALLER" | cut -d: -f1)"
AUTH_HEADER_LINE="$(grep -Fn '$ApiHeaders["Authorization"] = "Bearer $env:GITHUB_TOKEN"' "$POWERSHELL_INSTALLER" | cut -d: -f1)"
API_CALL_LINE="$(grep -Fn 'Invoke-RestMethod -Uri $ReleaseUrl -Headers $ApiHeaders' "$POWERSHELL_INSTALLER" | cut -d: -f1)"
[ -n "$TOKEN_BASE_LINE" ] && [ -n "$AUTH_HEADER_LINE" ] && [ -n "$API_CALL_LINE" ]
[ "$TOKEN_BASE_LINE" -lt "$AUTH_HEADER_LINE" ]
[ "$AUTH_HEADER_LINE" -lt "$API_CALL_LINE" ]
printf 'install.sh focused behavior tests passed (11 install attempts, 1 identity mismatch rejection, 4 PowerShell identity contracts, 1 umask-hardening install, 6 PowerShell token-scoping contracts)\n'
