#!/usr/bin/env bash
# Build an installable .deb for the rpi desktop shell.
#
# Uses only dpkg-deb/ar from the host: no packaging dependency is added. The
# package installs the Electron runtime plus this app under /opt/rpi-desktop,
# a launcher at /usr/bin/rpi-desktop, and a desktop entry. There are no
# maintainer scripts and nothing runs at install time beyond unpacking.
#
# Usage: bash crates/pi-cli/desktop/package-deb.sh [output-dir]
set -euo pipefail

here="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
package=rpi-desktop
version="${VERSION:-$(node -p "require('$here/package.json').version")}"
electron_version="$(node -p "require('$here/node_modules/electron/package.json').version")"
arch="$(dpkg --print-architecture)"
case "$arch" in
  amd64|arm64) ;;
  *) echo "unsupported Debian architecture: $arch" >&2; exit 1 ;;
esac
out_dir="${1:-$here/dist-deb}"
runtime="$here/node_modules/electron/dist"
[ -x "$runtime/electron" ] || { echo "missing Electron runtime: $runtime" >&2; exit 1; }
[ -f "$here/../web/public/rpi-logo.svg" ] || { echo "missing icon source" >&2; exit 1; }
binary="$here/../../../target/release-dist/rpi"
[ -x "$binary" ] || binary="$here/../../../target/release/rpi"
[ -x "$binary" ] || { echo "missing release rpi: build target/release-dist/rpi or target/release/rpi first" >&2; exit 1; }

# Depends are derived from the libraries the runtime actually links, mapped to
# the packages this distribution ships them in (Ubuntu 24.04 names libasound2t64,
# libgtk-3-0t64, ... rather than the pre-t64 names).
if [ -z "${DEPENDS:-}" ]; then
  DEPENDS="$(ldd "$runtime/electron" | sed -n 's/^\s*[^ ]* => \(\/[^ ]*\).*/\1/p' | sort -u \
    | while read -r lib; do dpkg-query -S "$(readlink -f "$lib")" 2>/dev/null | sed -n 's/:.*//p'; done \
    | sort -u | grep -v '^firefox$' | tr '\n' ',' | sed 's/,$//; s/,/, /g')"
fi
depends="$DEPENDS"

(cd "$here" && rm -rf dist && npm run build >/dev/null)

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
chmod 0755 "$stage"

install -d -m 0755 "$stage/opt/$package" "$stage/opt/$package/resources/app"
cp -a "$runtime/." "$stage/opt/$package/"
rm -rf "$stage/opt/$package/resources/default_app.asar"
cp -a "$here/dist" "$stage/opt/$package/resources/app/dist"
manifest="$(mktemp)"
VERSION="$version" node -e 'const fs=require("fs"); const pkg=require(process.argv[1]); pkg.version=process.env.VERSION; fs.writeFileSync(process.argv[2], JSON.stringify(pkg, null, 2)+"\n");' "$here/package.json" "$manifest"
install -m 0644 "$manifest" "$stage/opt/$package/resources/app/package.json"
rm -f "$manifest"
install -m 0644 "$here/README.md" "$stage/opt/$package/resources/app/README.md"
install -m 0755 "$binary" "$stage/opt/$package/resources/rpi"

# The audit's mode contract: no special bits except the setuid sandbox helper.
find "$stage/opt/$package" \( -type l -o -type p -o -type s -o -type b -o -type c \) -delete
find "$stage/opt/$package" -type d -exec chmod 0755 {} +
find "$stage/opt/$package" -type f -exec chmod 0644 {} +
chmod 0755 "$stage/opt/$package/electron" "$stage/opt/$package/chrome_crashpad_handler"
chmod 0755 "$stage/opt/$package/resources/rpi"
chmod 4755 "$stage/opt/$package/chrome-sandbox"

install -d -m 0755 "$stage/usr/bin" "$stage/usr/share/applications" "$stage/usr/share/icons/hicolor/scalable/apps"
cat > "$stage/usr/bin/rpi-desktop" <<'LAUNCHER'
#!/bin/sh
# Fixed paths only: no PATH lookup, no shell evaluation of arguments.
exec /opt/rpi-desktop/electron /opt/rpi-desktop/resources/app "$@"
LAUNCHER
chmod 0755 "$stage/usr/bin/rpi-desktop"
cat > "$stage/usr/share/applications/rpi-desktop.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=rpi desktop
Comment=rpi listener web and chat client
Exec=/usr/bin/rpi-desktop
TryExec=/usr/bin/rpi-desktop
Terminal=false
Categories=Development;Utility;
Icon=rpi-desktop
DESKTOP
chmod 0644 "$stage/usr/share/applications/rpi-desktop.desktop"
install -m 0644 "$here/../web/public/rpi-logo.svg" "$stage/usr/share/icons/hicolor/scalable/apps/rpi-desktop.svg"

install -d -m 0755 "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<CONTROL
Package: $package
Version: $version
Architecture: $arch
Maintainer: rpi <rpi@localhost>
Depends: $depends
Section: utils
Priority: optional
Installed-Size: $(du -sk "$stage/opt" "$stage/usr" | awk '{sum += $1} END {print sum}')
Description: rpi desktop shell
 Electron $electron_version shell with an embedded release runner and a
 sandboxed, origin-scoped native menu bridge for /web and /chat.
CONTROL
chmod 0644 "$stage/DEBIAN/control"
(cd "$stage" && find opt usr -type f -exec md5sum {} + | sort -k2 > DEBIAN/md5sums)
chmod 0644 "$stage/DEBIAN/md5sums"

# Self-check: the archive carries only control + md5sums, every entry is 0:0,
# and the only privileged file is the verified sandbox helper.
[ "$(find "$stage/DEBIAN" -maxdepth 1 -type f | wc -l)" -eq 2 ] || { echo "DEBIAN holds more than control+md5sums" >&2; exit 1; }
[ -z "$(find "$stage" \( -type l -o -type p -o -type s -o -type b -o -type c \) -print -quit)" ] || { echo "payload holds a non-regular entry" >&2; exit 1; }
[ -z "$(find "$stage" -type f ! -perm 0644 ! -perm 0755 ! -perm 4755 -print -quit)" ] || { echo "payload holds an unexpected file mode" >&2; exit 1; }
[ "$(find "$stage" -type f -perm 4755 | wc -l)" -eq 1 ] || { echo "expected exactly one 4755 file" >&2; exit 1; }
[ -z "$(find "$stage" -type f \( -perm -g+w -o -perm -o+w \) -print -quit)" ] || { echo "payload holds a group/world-writable file" >&2; exit 1; }
[ -z "$(find "$stage" -type d ! -perm 0755 -print -quit)" ] || { echo "payload holds an unexpected directory mode" >&2; exit 1; }

mkdir -p "$out_dir"
deb="$out_dir/${package}_${version}_${arch}.deb"
dpkg-deb --build --root-owner-group "$stage" "$deb" >/dev/null
echo "$deb"
