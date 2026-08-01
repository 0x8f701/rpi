#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../lib/common.sh
. "$SCRIPT_DIR/../lib/common.sh"
list_scenarios() {
    printf '%s\n' \
        'release.inventory - exact five-platform archive plus SHA256SUMS inventory' \
        'release.checksums - verify every published archive digest' \
        'release.members - verify root rpi/rpi.exe and LICENSE in each archive'
}
case "${1:-list}" in
    list|--list|--dry-run) list_scenarios; exit 0 ;;
    run) ;;
    *) fail "usage: $0 [run VERSION DIST_DIR|list|--dry-run]" ;;
esac
[ "$#" -eq 3 ] || fail "usage: $0 run VERSION DIST_DIR"
version="${2#v}"
dist="$(CDPATH= cd -- "$3" && pwd -P)"
require_cmd tar
[ -f "$dist/SHA256SUMS" ] || fail "missing $dist/SHA256SUMS"
expected="$WORK_ROOT/release-expected.txt"; actual="$WORK_ROOT/release-actual.txt"
prepare_roots
printf '%s\n' \
    SHA256SUMS \
    "rpi-$version-aarch64-apple-darwin.tar.gz" \
    "rpi-$version-aarch64-unknown-linux-gnu.tar.gz" \
    "rpi-$version-x86_64-apple-darwin.tar.gz" \
    "rpi-$version-x86_64-pc-windows-msvc.zip" \
    "rpi-$version-x86_64-unknown-linux-gnu.tar.gz" | LC_ALL=C sort > "$expected"
(
    cd "$dist"
    printf '%s\n' ./* | sed 's#^./##' | LC_ALL=C sort
) > "$actual"
diff -u "$expected" "$actual"
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$dist" && sha256sum -c SHA256SUMS)
elif command -v shasum >/dev/null 2>&1; then
    while read -r digest name; do
        [ "$(shasum -a 256 "$dist/${name#\*}" | cut -d ' ' -f 1)" = "$digest" ] || fail "checksum mismatch: $name"
    done < "$dist/SHA256SUMS"
else
    fail "missing sha256sum or shasum"
fi
for archive in "$dist"/*.tar.gz; do
    listing="$WORK_ROOT/$(basename "$archive").list"
    tar -tzf "$archive" > "$listing"
    grep -Eq '^\./?rpi$|^rpi$' "$listing" || fail "$(basename "$archive") has no root rpi"
    grep -Eq '^\./?LICENSE$|^LICENSE$' "$listing" || fail "$(basename "$archive") has no root LICENSE"
done
if command -v python3 >/dev/null 2>&1; then
    python3 - "$dist/rpi-$version-x86_64-pc-windows-msvc.zip" <<'PY'
import sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as archive:
    names={name.removeprefix('./') for name in archive.namelist()}
assert 'rpi.exe' in names, names
assert 'LICENSE' in names, names
PY
else
    fail "python3 is required to inspect the Windows zip"
fi
printf 'release archive inventory passed\ndist=%s\n' "$dist"
