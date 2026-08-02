#!/bin/sh
#
# rpi installer (macOS / Linux).
#
# Downloads the matching platform artifact from this repo's GitHub Releases,
# verifies its SHA-256 against the release's SHA256SUMS manifest, and installs
# the binary as ~/.rpi/bin/rpi (versioned binary in ~/.rpi/downloads/,
# atomic symlink in bin/).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/0x8f701/rpi/master/install.sh | sh
#   sh install.sh --version v0.2.2      # pin a specific release
#
# Environment:
#   PI_HOME                install root (default: ~/.rpi)
#   PI_UPDATE_BASE_URL     GitHub-Releases-shaped API base (default:
#                          https://api.github.com/repos/0x8f701/rpi/releases)
#
# Fails fast on any error; never leaves a partial binary as the active rpi.

set -eu

REPO="0x8f701/rpi"
API_BASE="${PI_UPDATE_BASE_URL:-https://api.github.com/repos/${REPO}/releases}"
PI_HOME="${PI_HOME:-$HOME/.rpi}"

err() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

usage() {
    sed -n '2,20p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'
}

is_semver() {
    printf '%s\n' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
}

# ── Arguments ────────────────────────────────────────────────────────────────
VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || err "--version requires a value"
            VERSION="$2"
            shift 2
            ;;
        --version=*)
            VERSION="${1#*=}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            err "unknown argument: $1"
            ;;
    esac
done
VERSION="${VERSION#v}"
if [ -n "$VERSION" ] && ! is_semver "$VERSION"; then
    err "invalid version '$VERSION' (expected X.Y.Z or vX.Y.Z)"
fi

# ── Platform detection ───────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"
PLATFORM_OS=""
PLATFORM_ARCH=""
TRIPLE=""
case "$OS" in
    Darwin)
        PLATFORM_OS="macos"
        case "$ARCH" in
            arm64|aarch64)
                PLATFORM_ARCH="aarch64"
                TRIPLE="aarch64-apple-darwin"
                ;;
            x86_64)
                PLATFORM_ARCH="x86_64"
                TRIPLE="x86_64-apple-darwin"
                ;;
            *)
                err "unsupported macOS architecture: $ARCH"
                ;;
        esac
        ;;
    Linux)
        PLATFORM_OS="linux"
        case "$ARCH" in
            aarch64|arm64)
                PLATFORM_ARCH="aarch64"
                TRIPLE="aarch64-unknown-linux-gnu"
                ;;
            x86_64|amd64)
                PLATFORM_ARCH="x86_64"
                TRIPLE="x86_64-unknown-linux-gnu"
                ;;
            *)
                err "unsupported Linux architecture: $ARCH"
                ;;
        esac
        ;;
    *)
        err "unsupported OS: $OS (Windows: use install.ps1)"
        ;;
esac

# ── Downloader ───────────────────────────────────────────────────────────────
# Optional, set GITHUB_TOKEN to authenticate the fixed GitHub API endpoint and
# avoid the unauthenticated rate limit (60 req/hr per IP). Never forward the
# token to release-asset hosts or a custom test endpoint.
AUTH_HDR=""
if [ -n "${GITHUB_TOKEN:-}" ]; then
    AUTH_HDR="Authorization: Bearer $GITHUB_TOKEN"
fi

is_fixed_github_api_url() {
    case "$1" in
        "https://api.github.com/repos/${REPO}/releases"*)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

if command -v curl >/dev/null 2>&1; then
    fetch() {
        if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$1"; then
            curl -fsSL -H "$AUTH_HDR" -o "$2" "$1" || return 1
        else
            curl -fsSL -o "$2" "$1" || return 1
        fi
    }
    fetch_stdout() {
        if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$1"; then
            curl -fsSL -H "$AUTH_HDR" "$1" || return 1
        else
            curl -fsSL "$1" || return 1
        fi
    }
elif command -v wget >/dev/null 2>&1; then
    fetch() {
        if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$1"; then
            wget -q --header="$AUTH_HDR" -O "$2" "$1" || return 1
        else
            wget -q -O "$2" "$1" || return 1
        fi
    }
    fetch_stdout() {
        if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$1"; then
            wget -q --header="$AUTH_HDR" -O - "$1" || return 1
        else
            wget -q -O - "$1" || return 1
        fi
    }
else
    err "neither curl nor wget found"
fi

# ── SHA-256 tool ─────────────────────────────────────────────────────────────
if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum -b "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 -b "$1" | awk '{print $1}'; }
else
    err "neither sha256sum nor shasum found"
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rpi-install.XXXXXX")"
STAGED=""
TMP_LINK=""
ROLLBACK_LINK=""
STATE_TMP=""
TRANSACTION_ACTIVE=0
ROLLBACK_RUNNING=0
cleanup() {
    trap - EXIT HUP INT TERM
    if [ "${TRANSACTION_ACTIVE:-0}" -eq 1 ] && [ "${ROLLBACK_RUNNING:-0}" -eq 0 ]; then
        ROLLBACK_RUNNING=1
        if rollback_install; then
            TRANSACTION_ACTIVE=0
        else
            printf 'install.sh: error: interrupted install rollback failed\n' >&2
        fi
    fi
    if [ -n "${STATE_TMP:-}" ] && [ -f "$STATE_TMP" ]; then rm -f "$STATE_TMP"; fi
    if [ -n "${ROLLBACK_LINK:-}" ] && { [ -e "$ROLLBACK_LINK" ] || [ -L "$ROLLBACK_LINK" ]; }; then rm -f "$ROLLBACK_LINK"; fi
    if [ -n "${TMP_LINK:-}" ] && { [ -e "$TMP_LINK" ] || [ -L "$TMP_LINK" ]; }; then rm -f "$TMP_LINK"; fi
    if [ -n "${STAGED:-}" ] && [ -f "$STAGED" ]; then rm -f "$STAGED"; fi
    if [ -d "$TMP_DIR" ]; then rm -rf "$TMP_DIR"; fi
    release_install_lock
}
trap cleanup EXIT
trap 'cleanup; exit 1' HUP INT TERM

# ── Resolve the release ──────────────────────────────────────────────────────
if [ -n "$VERSION" ]; then
    RELEASE_URL="$API_BASE/tags/v$VERSION"
else
    RELEASE_URL="$API_BASE/latest"
fi
printf 'Resolving release from %s\n' "$RELEASE_URL"
RELEASE_JSON="$(fetch_stdout "$RELEASE_URL")" \
    || err "could not fetch release metadata from $RELEASE_URL
         (GitHub may be rate-limiting this IP; set GITHUB_TOKEN to authenticate)"

TAG="$(printf '%s' "$RELEASE_JSON" \
    | sed 's/"tag_name"/\
"tag_name"/g' \
    | sed -n 's/^[[:space:]]*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1)"
[ -n "$TAG" ] || err "release metadata has no tag_name (endpoint: $RELEASE_URL)"
case "$TAG" in
    v*)
        RESOLVED_VERSION="${TAG#v}"
        ;;
    *)
        err "release tag '$TAG' is invalid (expected vX.Y.Z)"
        ;;
esac
is_semver "$RESOLVED_VERSION" \
    || err "release tag '$TAG' is invalid (expected semantic version vX.Y.Z)"
if [ -n "$VERSION" ] && [ "$RESOLVED_VERSION" != "$VERSION" ]; then
    err "requested version $VERSION but release tag is $TAG"
fi

URLS="$(printf '%s' "$RELEASE_JSON" \
    | sed 's/"browser_download_url"/\
"browser_download_url"/g' \
    | sed -n 's/^[[:space:]]*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"

find_asset_url() {
    suffix="$1"
    found=""
    count=0
    for u in $URLS; do
        case "$u" in
            */"$suffix")
                if [ "$count" -ne 0 ]; then
                    err "release $TAG contains duplicate $suffix assets"
                fi
                found="$u"
                count=1
                ;;
        esac
    done
    if [ "$count" -ne 1 ]; then
        return 1
    fi
    printf '%s\n' "$found"
}

if ! SUMS_URL="$(find_asset_url "SHA256SUMS")"; then
    err "release $TAG must contain exactly one SHA256SUMS asset"
fi

ASSET="rpi-${RESOLVED_VERSION}-${TRIPLE}.tar.gz"
if ! ARCHIVE_URL="$(find_asset_url "$ASSET")"; then
    err "release $TAG does not contain asset $ASSET"
fi

# ── Download + verify ────────────────────────────────────────────────────────
printf 'Downloading rpi v%s (%s)...\n' "$RESOLVED_VERSION" "$TRIPLE"
fetch "$ARCHIVE_URL" "$TMP_DIR/$ASSET" || err "download failed: $ARCHIVE_URL"
fetch "$SUMS_URL" "$TMP_DIR/SHA256SUMS" || err "download failed: $SUMS_URL"

MANIFEST_SIZE="$(wc -c < "$TMP_DIR/SHA256SUMS" | tr -d '[:space:]')"
ARCHIVE_SIZE="$(wc -c < "$TMP_DIR/$ASSET" | tr -d '[:space:]')"
[ "$MANIFEST_SIZE" -le 1048576 ] || err "SHA256SUMS is unexpectedly large"
[ "$ARCHIVE_SIZE" -le 1073741824 ] || err "$ASSET exceeds the 1 GiB safety limit"

EXPECTED=""
EXPECTED_COUNT=0
while IFS=' ' read -r hash name; do
    [ -n "$hash" ] || continue
    case "$name" in
        "$ASSET"|"*$ASSET")
            EXPECTED="$hash"
            EXPECTED_COUNT=$((EXPECTED_COUNT + 1))
            ;;
    esac
done < "$TMP_DIR/SHA256SUMS"
[ "$EXPECTED_COUNT" -eq 1 ] \
    || err "SHA256SUMS must contain exactly one entry for $ASSET"
case "$EXPECTED" in
    *[!0-9A-Fa-f]*|'')
        err "SHA256SUMS contains an invalid digest for $ASSET"
        ;;
esac
[ "${#EXPECTED}" -eq 64 ] || err "SHA256SUMS contains an invalid digest for $ASSET"
EXPECTED="$(printf '%s' "$EXPECTED" | tr 'A-F' 'a-f')"
ACTUAL="$(sha256_of "$TMP_DIR/$ASSET" | tr 'A-F' 'a-f')"
if [ "$ACTUAL" != "$EXPECTED" ]; then
    err "SHA256 mismatch for $ASSET: expected $EXPECTED, got $ACTUAL"
fi
printf 'Checksum verified.\n'

# ── Extract + install ────────────────────────────────────────────────────────
tar -tzf "$TMP_DIR/$ASSET" > "$TMP_DIR/archive.list" \
    || err "failed to inspect $ASSET"

BINARY_MEMBER=""
BINARY_COUNT=0
while IFS= read -r member; do
    normalized="${member#./}"
    [ "$normalized" = "rpi" ] || continue
    case "$member" in
        */)
            err "archive $ASSET contains a directory binary entry: $member"
            ;;
    esac
    if [ "$BINARY_COUNT" -ne 0 ]; then
        err "archive $ASSET contains more than one root-level rpi binary"
    fi
    BINARY_MEMBER="$member"
    BINARY_COUNT=1
done < "$TMP_DIR/archive.list"
[ "$BINARY_COUNT" -eq 1 ] \
    || err "archive $ASSET must contain exactly one root-level rpi binary"

tar -xOzf "$TMP_DIR/$ASSET" "$BINARY_MEMBER" > "$TMP_DIR/rpi" \
    || err "failed to extract rpi from $ASSET"
[ -s "$TMP_DIR/rpi" ] || err "archive $ASSET contains an empty rpi binary"
BINARY_SIZE="$(wc -c < "$TMP_DIR/rpi" | tr -d '[:space:]')"
[ "$BINARY_SIZE" -le 1073741824 ] || err "extracted rpi exceeds the 1 GiB safety limit"
chmod 0755 "$TMP_DIR/rpi"

ensure_directory() {
    path="$1"
    label="$2"
    [ ! -L "$path" ] || err "refusing to use symlinked $label: $path"
    if [ -e "$path" ]; then
        [ -d "$path" ] || err "$label is not a directory: $path"
    else
        mkdir -p "$path" || err "could not create $label: $path"
    fi
}

DOWNLOADS_DIR="$PI_HOME/downloads"
BIN_DIR="$PI_HOME/bin"
STATE_FILE="$PI_HOME/update-state.json"
ensure_directory "$PI_HOME" "rpi install root"
ensure_directory "$DOWNLOADS_DIR" "rpi downloads directory"
ensure_directory "$BIN_DIR" "rpi bin directory"

# ── Serialize concurrent installs ─────────────────────────────────────────────
# The content-addressed versioned path is shared by every install of the same
# release, so two concurrent installs of the same release would race on DEST
# and the active symlink. Hold an exclusive lock over PI_HOME for the whole
# transaction. The portable PID lock reaps dead/corrupt owners, but never waits
# more than 30 seconds for a live owner, matching the self-updater deadline.
LOCKFILE="$PI_HOME/.install.lock"
LOCK_WAIT_SECONDS=30
[ ! -L "$LOCKFILE" ] || err "refusing to use a symlinked install lock: $LOCKFILE"
LOCK_HELD=0
LOCK_OWNER="unknown"
acquire_install_lock() {
    waited=0
    while :; do
        if ( set -C; printf '%s\n' "$$" > "$LOCKFILE" ) 2>/dev/null; then
            LOCK_HELD=1
            return 0
        fi
        owner="$(cat "$LOCKFILE" 2>/dev/null || true)"
        case "$owner" in
            ''|*[!0-9]*)
                # Corrupt or empty lock; remove it only if possible.
                if rm -f "$LOCKFILE" 2>/dev/null; then
                    continue
                fi
                LOCK_OWNER="invalid owner"
                ;;
            *)
                if [ "$owner" = "$$" ]; then
                    LOCK_HELD=1
                    return 0
                fi
                if ! kill -0 "$owner" 2>/dev/null; then
                    # Stale lock from a dead process; remove it only if possible.
                    if rm -f "$LOCKFILE" 2>/dev/null; then
                        continue
                    fi
                    LOCK_OWNER="$owner (stale)"
                else
                    LOCK_OWNER="$owner"
                fi
                ;;
        esac
        if [ "$waited" -ge "$LOCK_WAIT_SECONDS" ]; then
            return 1
        fi
        if [ "$waited" -eq 0 ]; then
            printf 'Waiting up to %s seconds for another rpi install (lock %s, owner %s)...\n' \
                "$LOCK_WAIT_SECONDS" "$LOCKFILE" "$LOCK_OWNER"
        fi
        sleep 1 || return 1
        waited=$((waited + 1))
    done
}
release_install_lock() {
    [ "${LOCK_HELD:-0}" = 1 ] || return 0
    [ -n "${LOCKFILE:-}" ] || return 0
    if [ -f "$LOCKFILE" ]; then
        owner="$(cat "$LOCKFILE" 2>/dev/null || true)"
        [ "$owner" = "$$" ] && rm -f "$LOCKFILE" 2>/dev/null
    fi
    LOCK_HELD=0
}
acquire_install_lock \
    || err "timed out after ${LOCK_WAIT_SECONDS}s waiting for another rpi install (lock $LOCKFILE, owner $LOCK_OWNER); retry after it finishes"


# Detect a legacy installer-managed `pi` command before replacing the shared
# update state. Removal is deferred until the verified rpi binary and its new
# state have both committed, so rollback always preserves the old command.
LEGACY_PI_PATH="$BIN_DIR/pi"
LEGACY_PI_TARGET=""
if [ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -L "$LEGACY_PI_PATH" ]; then
    LEGACY_VERSION="$(sed -n 's/^[[:space:]]*"installed_version"[[:space:]]*:[[:space:]]*"\([^"]*\)"[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$STATE_FILE")"
    LEGACY_ASSET="$(sed -n 's/^[[:space:]]*"installed_asset"[[:space:]]*:[[:space:]]*"\([^"]*\)"[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$STATE_FILE")"
    LEGACY_DIGEST="$(sed -n 's/^[[:space:]]*"installed_sha256"[[:space:]]*:[[:space:]]*"\([^"]*\)"[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$STATE_FILE")"
    LEGACY_BINARY="$(sed -n 's/^[[:space:]]*"installed_binary"[[:space:]]*:[[:space:]]*"\([^"]*\)"[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$STATE_FILE")"
    case "$LEGACY_DIGEST" in
        *[!0-9A-Fa-f]*|'') LEGACY_DIGEST_VALID=0 ;;
        *) [ "${#LEGACY_DIGEST}" -eq 64 ] && LEGACY_DIGEST_VALID=1 || LEGACY_DIGEST_VALID=0 ;;
    esac
    EXPECTED_LEGACY_ASSET="pi-rs-${LEGACY_VERSION}-${TRIPLE}.tar.gz"
    EXPECTED_LEGACY_BINARY="pi-rs-${LEGACY_VERSION}-${PLATFORM_OS}-${PLATFORM_ARCH}-sha256-${LEGACY_DIGEST}"
    CURRENT_LEGACY_TARGET="$(readlink "$LEGACY_PI_PATH" 2>/dev/null || true)"
    if is_semver "$LEGACY_VERSION" \
        && [ "$LEGACY_DIGEST_VALID" -eq 1 ] \
        && [ "$LEGACY_ASSET" = "$EXPECTED_LEGACY_ASSET" ] \
        && [ "$LEGACY_BINARY" = "$EXPECTED_LEGACY_BINARY" ] \
        && [ "$CURRENT_LEGACY_TARGET" = "../downloads/$LEGACY_BINARY" ] \
        && [ -f "$DOWNLOADS_DIR/$LEGACY_BINARY" ] \
        && [ ! -L "$DOWNLOADS_DIR/$LEGACY_BINARY" ]; then
        LEGACY_PI_TARGET="$CURRENT_LEGACY_TARGET"
    fi
fi

# The archive digest is part of the deployment identity. A deliberately
# republished tag therefore gets a new path and cannot overwrite the active
# same-semver binary before its smoke test succeeds.
VERSIONED="rpi-${RESOLVED_VERSION}-${PLATFORM_OS}-${PLATFORM_ARCH}-sha256-${EXPECTED}"
DEST="$DOWNLOADS_DIR/$VERSIONED"
STAGED="$(mktemp "$DOWNLOADS_DIR/.rpi-stage.XXXXXX")" \
    || err "could not create a staged binary under $DOWNLOADS_DIR"
cp "$TMP_DIR/rpi" "$STAGED" || err "could not stage downloaded rpi"
chmod 0755 "$STAGED"
# Smoke-test the staged bytes before touching either live component.
"$STAGED" --version >/dev/null 2>&1 \
    || err "downloaded binary failed smoke test; existing install left untouched"

TMP_LINK="$BIN_DIR/rpi.install.$$"
[ ! -e "$TMP_LINK" ] && [ ! -L "$TMP_LINK" ] \
    || { rm -f "$STAGED"; err "temporary activation path already exists: $TMP_LINK"; }
ln -s "../downloads/$VERSIONED" "$TMP_LINK" \
    || { rm -f "$STAGED"; err "failed to stage active rpi link"; }

if [ ! -L "$BIN_DIR/rpi" ] && [ -e "$BIN_DIR/rpi" ]; then
    rm -f "$TMP_LINK" "$STAGED"
    err "$BIN_DIR/rpi is not a managed symlink; refusing to overwrite it"
fi
if [ -L "$BIN_DIR/rpi" ] && [ -d "$BIN_DIR/rpi" ]; then
    rm -f "$TMP_LINK" "$STAGED"
    err "$BIN_DIR/rpi is a symlink to a directory; refusing unsafe activation"
fi

# Capture the prior active symlink target so rollback restores exactly what
# was live before this install, not the newly staged version.
HAD_ACTIVE=0
OLD_LINK_TARGET=""
if [ -L "$BIN_DIR/rpi" ]; then
    OLD_LINK_TARGET="$(readlink "$BIN_DIR/rpi")" \
        || { rm -f "$TMP_LINK" "$STAGED"; err "failed to read prior active rpi symlink"; }
    HAD_ACTIVE=1
fi

# Capture whether a same-identity versioned binary already exists. The
# versioned path is content-addressed by the archive SHA-256, so a pre-existing
# DEST always carries byte-identical content; activation replaces it with a
# single atomic rename and rollback leaves it in place (the prior symlink may
# still reference it).
HAD_DEST=0
if [ -e "$DEST" ] || [ -L "$DEST" ]; then
    [ ! -L "$DEST" ] || err "refusing to replace a symlinked versioned binary: $DEST"
    [ -f "$DEST" ] || err "versioned binary path is not a regular file: $DEST"
    HAD_DEST=1
fi

# Restore the prior managed symlink with an atomic rename, then verify the
# live link contains the exact target captured before activation.
rollback_active_link() {
    if [ "$HAD_ACTIVE" -eq 1 ]; then
        ROLLBACK_LINK="$BIN_DIR/rpi.rollback.$$"
        if [ -e "$ROLLBACK_LINK" ] || [ -L "$ROLLBACK_LINK" ]; then
            printf 'install.sh: error: rollback path already exists: %s\n' "$ROLLBACK_LINK" >&2
            return 1
        fi
        if ! ln -s "$OLD_LINK_TARGET" "$ROLLBACK_LINK"; then
            printf 'install.sh: error: rollback failed to stage the prior active rpi symlink\n' >&2
            return 1
        fi
        if ! mv -f "$ROLLBACK_LINK" "$BIN_DIR/rpi"; then
            printf 'install.sh: error: rollback failed to restore the prior active rpi symlink\n' >&2
            if ! rm -f "$ROLLBACK_LINK"; then
                printf 'install.sh: error: rollback also failed to remove temporary symlink: %s\n' "$ROLLBACK_LINK" >&2
            fi
            return 1
        fi
        if [ ! -L "$BIN_DIR/rpi" ]; then
            printf 'install.sh: error: rollback verification found no active rpi symlink\n' >&2
            return 1
        fi
        if ! RESTORED_LINK_TARGET="$(readlink "$BIN_DIR/rpi")"; then
            printf 'install.sh: error: rollback could not read the restored active rpi symlink\n' >&2
            return 1
        fi
        if [ "$RESTORED_LINK_TARGET" != "$OLD_LINK_TARGET" ]; then
            printf 'install.sh: error: rollback restored the wrong active rpi target\n' >&2
            return 1
        fi
    else
        if ! rm -f "$BIN_DIR/rpi"; then
            printf 'install.sh: error: rollback failed to remove the newly activated rpi symlink\n' >&2
            return 1
        fi
        if [ -e "$BIN_DIR/rpi" ] || [ -L "$BIN_DIR/rpi" ]; then
            printf 'install.sh: error: rollback verification found an unexpected active rpi path\n' >&2
            return 1
        fi
    fi
    return 0
}

rollback_install() {
    ROLLBACK_FAILED=0
    # Restore the active symlink BEFORE touching the versioned binary. After a
    # successful symlink swap (the state-write failure path), BIN_DIR/rpi points
    # at the new DEST, so removing DEST first would dangle the live link. Restoring
    # the prior symlink first makes the new DEST unreferenced, and only then is it
    # safe to remove. (After a failed swap, BIN_DIR/rpi is already the prior link,
    # so restoring it is an atomic no-op and DEST is already unreferenced.)
    if ! rollback_active_link; then
        ROLLBACK_FAILED=1
    fi
    # The atomic rename already placed the smoke-tested, checksum-verified
    # bytes at DEST. Only undo a DEST we created fresh; a pre-existing DEST
    # held byte-identical content and the prior symlink may still reference it.
    if [ "$HAD_DEST" -eq 0 ]; then
        if ! rm -f "$DEST"; then
            printf 'install.sh: error: rollback failed to remove new versioned binary: %s\n' "$DEST" >&2
            ROLLBACK_FAILED=1
        elif [ -e "$DEST" ] || [ -L "$DEST" ]; then
            printf 'install.sh: error: rollback verification found an unexpected versioned binary: %s\n' "$DEST" >&2
            ROLLBACK_FAILED=1
        fi
    fi
    [ "$ROLLBACK_FAILED" -eq 0 ]
}

fail_after_rollback() {
    ROLLBACK_CAUSE="$1"
    ROLLBACK_RUNNING=1
    if rollback_install; then
        TRANSACTION_ACTIVE=0
        err "$ROLLBACK_CAUSE; previous install restored"
    fi
    err "$ROLLBACK_CAUSE; rollback failed"
}

TRANSACTION_ACTIVE=1

# Atomic activation, step 1: place the smoke-tested bytes at the versioned
# path. `mv -f` is a single rename(2) on the same filesystem (STAGED lives under
# DOWNLOADS_DIR); it either creates DEST or atomically replaces its contents.
# There is no instant at which DEST is absent, so the active symlink — which
# may still point at DEST from a prior same-release install — never dangles,
# even under signal or crash between this step and the symlink swap below.
# A failure here leaves DEST untouched, so there is nothing to roll back.
if ! mv -f "$STAGED" "$DEST"; then
    rm -f "$TMP_LINK"
    err "failed to activate versioned binary"
fi
if [ ! -f "$DEST" ] || [ -L "$DEST" ]; then
    rm -f "$TMP_LINK"
    err "activated versioned binary is not a regular file: $DEST"
fi
STAGED=""

# Atomic activation, step 2: swap the active symlink. rename(2) of the symlink
# is atomic; BIN_DIR/rpi is either the prior target or the new one — never
# missing. A failure here leaves DEST holding the new bytes and the symlink
# unchanged, which we roll back to the prior symlink state.
if ! mv -f "$TMP_LINK" "$BIN_DIR/rpi"; then
    fail_after_rollback "failed to activate rpi binary"
fi
EXPECTED_ACTIVE_TARGET="../downloads/$VERSIONED"
ACTIVE_TARGET="$(readlink "$BIN_DIR/rpi" 2>/dev/null || true)"
if [ ! -L "$BIN_DIR/rpi" ] || [ -d "$BIN_DIR/rpi" ] \
    || [ ! -f "$BIN_DIR/rpi" ] || [ "$ACTIVE_TARGET" != "$EXPECTED_ACTIVE_TARGET" ]; then
    fail_after_rollback "activated rpi path failed symlink verification"
fi
TMP_LINK=""

# Record the exact release-archive identity used by any in-place updater so
# republished tags are detected by checksum.
# The shared state file now describes only the active rpi installation.
if [ -d "$STATE_FILE" ]; then
    fail_after_rollback "update-state path is a directory: $STATE_FILE"
fi

if ! STATE_TMP="$(mktemp "$PI_HOME/.update-state.XXXXXX")"; then
    STATE_TMP=""
    fail_after_rollback "could not create temporary update state under $PI_HOME"
fi

if ! CHECKED_AT="$(date -u +%s)"; then
    fail_after_rollback "could not determine the current Unix timestamp"
fi
case "$CHECKED_AT" in
    *[!0-9]*|'')
        fail_after_rollback "could not determine the current Unix timestamp"
        ;;
esac
printf '{\n  "installed_version": "%s",\n  "installed_asset": "%s",\n  "installed_sha256": "%s",\n  "installed_binary": "%s",\n  "checked_at_unix": %s\n}\n' \
    "$RESOLVED_VERSION" "$ASSET" "$EXPECTED" "$VERSIONED" "$CHECKED_AT" > "$STATE_TMP" || {
    fail_after_rollback "could not write update state"
}
if ! mv -f "$STATE_TMP" "$STATE_FILE"; then
    fail_after_rollback "could not record rpi update state"
fi
STATE_TMP=""
TRANSACTION_ACTIVE=0

# Clean-cutover migration: remove only the legacy command proven above to be
# installer-managed and still unchanged. Any unmanaged or raced path is kept.
if [ -n "$LEGACY_PI_TARGET" ] && [ -L "$LEGACY_PI_PATH" ]; then
    CURRENT_LEGACY_TARGET="$(readlink "$LEGACY_PI_PATH" 2>/dev/null || true)"
    if [ "$CURRENT_LEGACY_TARGET" = "$LEGACY_PI_TARGET" ] \
        && [ -f "$DOWNLOADS_DIR/$LEGACY_BINARY" ] \
        && [ ! -L "$DOWNLOADS_DIR/$LEGACY_BINARY" ]; then
        if rm -f "$LEGACY_PI_PATH" && [ ! -e "$LEGACY_PI_PATH" ] && [ ! -L "$LEGACY_PI_PATH" ]; then
            printf 'Removed legacy installer-managed command %s.\n' "$LEGACY_PI_PATH"
        else
            printf 'install.sh: warning: rpi installed, but legacy managed command could not be removed: %s\n' "$LEGACY_PI_PATH" >&2
        fi
    else
        printf 'install.sh: warning: legacy pi path changed during install; leaving it untouched: %s\n' "$LEGACY_PI_PATH" >&2
    fi
fi


printf '\nrpi v%s installed to %s\n' "$RESOLVED_VERSION" "$BIN_DIR/rpi"

case ":$PATH:" in
    *":$BIN_DIR:"*)
        printf 'Run `rpi` to get started.\n'
        ;;
    *)
        # Persist BIN_DIR on PATH in the login shell's rc file.  The install
        # itself is already committed; a failure here only means the user must
        # add the line manually.
        persist_line() {
            rc="$1"
            line="$2"
            if [ -f "$rc" ] && grep -qF "$line" "$rc"; then
                printf '\n%s is already configured in %s.\n' "$line" "$rc"
                return 0
            fi
            if printf '\n# Added by the rpi installer\n%s\n' "$line" >> "$rc"; then
                printf '\nAdded %s to your PATH in %s.\n' "$line" "$rc"
            else
                printf '\ninstall.sh: warning: could not write %s.\n' "$rc" >&2
                printf 'Add this line to your shell profile manually:\n  %s\n' "$line"
            fi
        }
        # POSIX single-quote escape: backslashes are literal; a single quote
        # is represented by closing the quote, adding an escaped quote, then
        # reopening. This keeps the generated rc line safe for any BIN_DIR.
        sh_quote() {
            printf '%s\n' "$1" | sed "s/'/'\\\\''/g"
        }
        SH_BIN_DIR="$(sh_quote "$BIN_DIR")"
        EXPORT_LINE="export PATH='$SH_BIN_DIR':\$PATH"
        # Fish single-quoted string: escape backslashes first, then single
        # quotes. In a single-quoted Fish string only \ and \' are special.
        fish_quote() {
            printf '%s' "$1" | sed 's/\\/\\\\/g; s/'"'"'/\\'"'"'/g'
        }
        FISH_DIR="$(fish_quote "$BIN_DIR")"
        case "${SHELL:-}" in
            */zsh)
                persist_line "${ZDOTDIR:-$HOME}/.zshrc" "$EXPORT_LINE"
                ;;
            */bash)
                if [ "$PLATFORM_OS" = "macos" ]; then
                    persist_line "$HOME/.bash_profile" "$EXPORT_LINE"
                else
                    persist_line "$HOME/.bashrc" "$EXPORT_LINE"
                fi
                ;;
            */fish)
                FISH_CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/fish"
                if mkdir -p "$FISH_CONF_DIR" 2>/dev/null; then
                    persist_line "$FISH_CONF_DIR/config.fish" "fish_add_path -- '$FISH_DIR'"
                else
                    printf '\ninstall.sh: warning: could not create %s.\n' "$FISH_CONF_DIR" >&2
                    printf 'Add this line to your fish config manually:\n  fish_add_path -- '\''%s'\''\n' "$FISH_DIR"
                fi
                ;;
            *)
                persist_line "$HOME/.profile" "$EXPORT_LINE"
                ;;
        esac
        printf 'Open a new terminal, then run `rpi` to get started.\n'
        ;;
esac
