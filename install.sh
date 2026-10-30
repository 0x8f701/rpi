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
#   sh install.sh --version v1.0.0      # pin a specific release
#
# Environment:
#   RPI_HOME                install root (default: ~/.rpi)
#   RPI_UPDATE_BASE_URL     GitHub-Releases-shaped API base (default:
#                          https://api.github.com/repos/0x8f701/rpi/releases)
#   GITHUB_TOKEN           authenticate the fixed GitHub API endpoint (default:
#                          none; never sent to release-asset hosts)
#   RPI_ALLOW_LOOPBACK_HTTP set to 1 to allow http:// on 127.0.0.1, localhost,
#                          or [::1] for a local release fixture. Never the
#                          default; every other host must be HTTPS.
#
# Fails fast on any error; never leaves a partial binary as the active rpi.

set -eu

REPO="0x8f701/rpi"
API_BASE="${RPI_UPDATE_BASE_URL:-https://api.github.com/repos/${REPO}/releases}"
RPI_HOME="${RPI_HOME:-$HOME/.rpi}"

err() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

# Legacy names are intentionally not aliased. Reject them with an actionable
# message before any network request or filesystem change so users do not
# silently install into the wrong root or leak credentials to the wrong endpoint.
if [ -n "${PI_HOME:-}" ]; then
    err "legacy PI_HOME is no longer supported; set RPI_HOME instead (current PI_HOME=$PI_HOME)"
fi
if [ -n "${PI_UPDATE_BASE_URL:-}" ]; then
    err "legacy PI_UPDATE_BASE_URL is no longer supported; set RPI_UPDATE_BASE_URL instead"
fi

usage() {
    sed -n '2,25p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'
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

url_scheme() {
    printf '%s' "$1" | sed -n 's/^\([A-Za-z][A-Za-z0-9+.-]*\):\/\/.*/\1/p' | tr 'A-Z' 'a-z'
}

url_authority() {
    printf '%s' "$1" | sed -n 's/^[A-Za-z][A-Za-z0-9+.-]*:\/\/\([^/?#]*\).*/\1/p'
}

url_host() {
    auth="$(url_authority "$1")"
    [ -n "$auth" ] || return 0
    case "$auth" in
        \[*\])
            printf '%s' "$auth" | sed 's/^\(\[[^]]*\]\).*/\1/'
            ;;
        *)
            printf '%s' "$auth" | sed 's/:.*//'
            ;;
    esac
}

is_loopback_host() {
    case "$1" in
        127.0.0.1|localhost|\[::1\])
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

is_loopback_http_url() {
    [ "$(url_scheme "$1")" = "http" ] || return 1
    is_loopback_host "$(url_host "$1")"
}

is_github_asset_host() {
    case "$1" in
        github.com|objects.githubusercontent.com|github-releases.githubusercontent.com|release-assets.githubusercontent.com)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

require_allowed_url() {
    candidate="$1"
    scheme="$(url_scheme "$candidate")"
    host="$(url_host "$candidate")"
    auth="$(url_authority "$candidate")"
    [ -n "$scheme" ] && [ -n "$host" ] && [ -n "$auth" ] \
        || err "refusing URL with a missing scheme or host: $candidate"
    case "$auth" in
        *@*)
            err "refusing URL with userinfo: $candidate"
            ;;
    esac
    case "$scheme" in
        https)
            return 0
            ;;
        http)
            if [ "${RPI_ALLOW_LOOPBACK_HTTP:-}" = "1" ] && is_loopback_host "$host"; then
                return 0
            fi
            err "refusing non-HTTPS URL (loopback http requires RPI_ALLOW_LOOPBACK_HTTP=1): $candidate"
            ;;
        *)
            err "refusing URL with scheme '$scheme' (HTTPS required): $candidate"
            ;;
    esac
}

require_effective_url() {
    requested="$1"
    effective="$2"
    [ -n "$effective" ] || err "refusing download with empty effective URL"
    require_allowed_url "$effective"
    req_host="$(url_host "$requested")"
    eff_host="$(url_host "$effective")"
    if is_fixed_github_api_url "$requested" || [ "$req_host" = "api.github.com" ]; then
        [ "$eff_host" = "api.github.com" ] \
            || err "release metadata redirected to an unexpected host: $effective"
        return 0
    fi
    if is_github_asset_host "$req_host"; then
        is_github_asset_host "$eff_host" \
            || err "download redirected to an unexpected host: $effective"
        return 0
    fi
    [ "$eff_host" = "$req_host" ] \
        || err "download redirected off the metadata host ($req_host): $effective"
}

allows_github_cdn_redirects() {
    is_fixed_github_api_url "$1" && return 0
    is_github_asset_host "$(url_host "$1")"
}

http_redirect_location() {
    hdr="$1"
    [ -f "$hdr" ] || return 1
    status="$(awk '$1 ~ /HTTP\// { code=$2 } END { print code }' "$hdr" | tr -d '\r')"
    case "$status" in
        301|302|303|307|308) ;;
        *)
            return 1
            ;;
    esac
    loc="$(awk 'tolower($1) == "location:" { loc=$2 } END { print loc }' "$hdr" | tr -d '\r')"
    [ -n "$loc" ] || return 1
    printf '%s' "$loc"
}

curl_fetch_pinned_redirects() {
    pinned_origin="$1"
    dest="$2"
    current="$pinned_origin"
    hop=0
    while [ "$hop" -le 5 ]; do
        require_allowed_url "$current"
        require_effective_url "$pinned_origin" "$current"
        hdr="$TMP_DIR/curl-headers.$$"
        code_file="$TMP_DIR/curl-http-code.$$"
        rm -f "$dest" "$hdr" "$code_file"
        if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$current"; then
            if ! curl -fsS --proto '=https' --proto-redir '=https' --max-redirs 0 \
                -H "$AUTH_HDR" -D "$hdr" -o "$dest" -w '%{http_code}' "$current" \
                > "$code_file"; then
                rm -f "$dest" "$hdr" "$code_file"
                return 1
            fi
        else
            if ! curl -fsS --proto '=https' --proto-redir '=https' --max-redirs 0 \
                -D "$hdr" -o "$dest" -w '%{http_code}' "$current" \
                > "$code_file"; then
                rm -f "$dest" "$hdr" "$code_file"
                return 1
            fi
        fi
        http_code="$(tr -d '\r\n' < "$code_file")"
        rm -f "$code_file"
        [ -n "$http_code" ] || {
            rm -f "$dest" "$hdr"
            err "refusing download with empty HTTP status"
        }
        case "$http_code" in
            200)
                rm -f "$hdr"
                return 0
                ;;
            301|302|303|307|308)
                loc="$(http_redirect_location "$hdr")" || loc=""
                rm -f "$hdr" "$dest"
                [ -n "$loc" ] || return 1
                current="$loc"
                hop=$((hop + 1))
                ;;
            *)
                rm -f "$dest" "$hdr"
                return 1
                ;;
        esac
    done
    return 1
}

safe_asset_name() {
    name="$1"
    case "$name" in
        ""|*/*|*\\*|*" "*)
            err "refusing asset name with unexpected characters: $name"
            ;;
    esac
    case "$name" in
        *[!A-Za-z0-9._-]*)
            err "refusing asset name with unexpected characters: $name"
            ;;
    esac
    printf '%s' "$name"
}

asset_url_for_name() {
    name="$(safe_asset_name "$1")"
    if is_fixed_github_api_url "$API_BASE"; then
        printf 'https://github.com/%s/releases/download/%s/%s' "$REPO" "$TAG" "$name"
        return 0
    fi
    origin_scheme="$(url_scheme "$API_BASE")"
    origin_auth="$(url_authority "$API_BASE")"
    printf '%s://%s/%s' "$origin_scheme" "$origin_auth" "$name"
}

if command -v curl >/dev/null 2>&1; then
    fetch_once() {
        url="$1"
        dest="$2"
        require_allowed_url "$url"
        if is_loopback_http_url "$url"; then
            if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$url"; then
                if ! curl -fsS --max-redirs 0 -H "$AUTH_HDR" -o "$dest" "$url"; then
                    return 1
                fi
            else
                if ! curl -fsS --max-redirs 0 -o "$dest" "$url"; then
                    return 1
                fi
            fi
            return 0
        fi
        if allows_github_cdn_redirects "$url"; then
            if curl_fetch_pinned_redirects "$url" "$dest"; then
                return 0
            fi
            return 1
        fi
        if ! curl -fsSL --proto '=https' --proto-redir '=https' --max-redirs 0 \
            -o "$dest" "$url"; then
            rm -f "$dest"
            return 1
        fi
        return 0
    }
elif command -v wget >/dev/null 2>&1; then
    fetch_once() {
        url="$1"
        dest="$2"
        require_allowed_url "$url"
        if is_loopback_http_url "$url"; then
            if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$url"; then
                wget -q --max-redirect=0 --header="$AUTH_HDR" -O "$dest" "$url"
            else
                wget -q --max-redirect=0 -O "$dest" "$url"
            fi
            return $?
        fi
        pinned_origin="$url"
        current="$url"
        hop=0
        while [ "$hop" -le 5 ]; do
            require_allowed_url "$current"
            require_effective_url "$pinned_origin" "$current"
            hdr="$TMP_DIR/wget-headers.$$"
            rm -f "$dest"
            if [ -n "$AUTH_HDR" ] && is_fixed_github_api_url "$current"; then
                if wget --https-only --max-redirect=0 --server-response \
                    --header="$AUTH_HDR" -O "$dest" "$current" 2>"$hdr"; then
                    rm -f "$hdr"
                    return 0
                fi
            else
                if wget --https-only --max-redirect=0 --server-response \
                    -O "$dest" "$current" 2>"$hdr"; then
                    rm -f "$hdr"
                    return 0
                fi
            fi
            loc="$(http_redirect_location "$hdr")" || loc=""
            rm -f "$hdr" "$dest"
            [ -n "$loc" ] || return 1
            current="$loc"
            hop=$((hop + 1))
        done
        return 1
    }
else
    err "neither curl nor wget found"
fi

fetch() {
    require_allowed_url "$1"
    attempt=1
    while ! fetch_once "$1" "$2"; do
        [ "$attempt" -lt 3 ] || return 1
        attempt=$((attempt + 1))
        printf 'Download attempt failed; retrying (%s/3)...\n' "$attempt" >&2
        sleep 1 || return 1
    done
}

fetch_stdout() {
    require_allowed_url "$1"
    meta="$TMP_DIR/release-meta.$$"
    attempt=1
    while ! fetch_once "$1" "$meta"; do
        [ "$attempt" -lt 3 ] || return 1
        attempt=$((attempt + 1))
        printf 'Request attempt failed; retrying (%s/3)...\n' "$attempt" >&2
        sleep 1 || return 1
    done
    cat "$meta"
    rm -f "$meta"
}

# ── SHA-256 tool ─────────────────────────────────────────────────────────────
if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum -b "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 -b "$1" | awk '{print $1}'; }
else
    err "neither sha256sum nor shasum found"
fi

release_install_lock() {
    [ "${LOCK_HELD:-0}" = 1 ] || return 0
    [ -n "${LOCKFILE:-}" ] || return 0
    if [ -f "$LOCKFILE" ]; then
        owner="$(cat "$LOCKFILE" 2>/dev/null || true)"
        [ "$owner" = "$$" ] && rm -f "$LOCKFILE" 2>/dev/null
    fi
    LOCK_HELD=0
}

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
require_allowed_url "$API_BASE"
if [ -n "$VERSION" ]; then
    RELEASE_URL="$API_BASE/tags/v$VERSION"
else
    RELEASE_URL="$API_BASE/latest"
fi
require_allowed_url "$RELEASE_URL"
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

release_lists_asset() {
    suffix="$1"
    count=0
    for u in $URLS; do
        case "$u" in
            */"$suffix")
                if [ "$count" -ne 0 ]; then
                    err "release $TAG contains duplicate $suffix assets"
                fi
                count=1
                ;;
        esac
    done
    [ "$count" -eq 1 ]
}

if ! release_lists_asset "SHA256SUMS"; then
    err "release $TAG must contain exactly one SHA256SUMS asset"
fi

ASSET="rpi-${RESOLVED_VERSION}-${TRIPLE}.tar.gz"
if ! release_lists_asset "$ASSET"; then
    err "release $TAG does not contain asset $ASSET"
fi

SUMS_URL="$(asset_url_for_name "SHA256SUMS")"
ARCHIVE_URL="$(asset_url_for_name "$ASSET")"

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
    # Managed install state is owner-only, independent of the caller's umask.
    chmod 0700 "$path" || err "could not secure $label permissions: $path"
}

DOWNLOADS_DIR="$RPI_HOME/downloads"
BIN_DIR="$RPI_HOME/bin"
STATE_FILE="$RPI_HOME/update-state.json"
ensure_directory "$RPI_HOME" "rpi install root"
ensure_directory "$DOWNLOADS_DIR" "rpi downloads directory"
ensure_directory "$BIN_DIR" "rpi bin directory"

# ── Serialize concurrent installs ─────────────────────────────────────────────
# The content-addressed versioned path is shared by every install of the same
# release, so two concurrent installs of the same release would race on DEST
# and the active symlink. Hold an exclusive lock over RPI_HOME for the whole
# transaction. The portable PID lock reaps dead/corrupt owners, but never waits
# more than 30 seconds for a live owner, matching the self-updater deadline.
LOCKFILE="$RPI_HOME/.install.lock"
LOCK_WAIT_SECONDS=30
[ ! -L "$LOCKFILE" ] || err "refusing to use a symlinked install lock: $LOCKFILE"
LOCK_HELD=0
LOCK_OWNER="unknown"
acquire_install_lock() {
    waited=0
    while :; do
        if ( set -C; printf '%s\n' "$$" > "$LOCKFILE" && chmod 0600 "$LOCKFILE" ) 2>/dev/null; then
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
# same-semver binary before its identity check succeeds.
VERSIONED="rpi-${RESOLVED_VERSION}-${PLATFORM_OS}-${PLATFORM_ARCH}-sha256-${EXPECTED}"
DEST="$DOWNLOADS_DIR/$VERSIONED"
STAGED="$(mktemp "$DOWNLOADS_DIR/.rpi-stage.XXXXXX")" \
    || err "could not create a staged binary under $DOWNLOADS_DIR"
cp "$TMP_DIR/rpi" "$STAGED" || err "could not stage downloaded rpi"
chmod 0755 "$STAGED"
# Identity is read from already-checksummed bytes; the candidate is never executed.
# Release-dist LTO does not store clap's runtime "rpi $VERSION" line as one C string.
EXPECTED_VERSION_OUTPUT="rpi $RESOLVED_VERSION"
if tr '\000' '\n' < "$STAGED" | grep -F -q -- "$EXPECTED_VERSION_OUTPUT"; then
    :
elif tr '\000' '\n' < "$STAGED" | grep -F -q -- "rpi$RESOLVED_VERSION"; then
    :
else
    err "downloaded binary reported unexpected identity/version (expected '$EXPECTED_VERSION_OUTPUT'); existing install left untouched"
fi

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

if ! STATE_TMP="$(mktemp "$RPI_HOME/.update-state.XXXXXX")"; then
    STATE_TMP=""
    fail_after_rollback "could not create temporary update state under $RPI_HOME"
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
# Owner-only updater state, independent of the caller's umask (mktemp already
# creates the temp 0600; this also tightens a pre-existing looser state file).
chmod 0600 "$STATE_FILE" || fail_after_rollback "could not secure rpi update state permissions"
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
