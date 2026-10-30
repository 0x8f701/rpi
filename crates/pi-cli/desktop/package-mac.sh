#!/usr/bin/env bash
# Stage the host Electron.app with this desktop shell and the release rpi binary.
#
# No packaging framework is added. The zip root is rpi-desktop.app, with LICENSE inside Contents/Resources.
# macOS Gatekeeper still applies: this script does not sign or notarize the app.
#
# Usage: VERSION=1.0.0 RPI_BINARY=/path/to/rpi bash package-mac.sh output.zip
set -euo pipefail

here="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
version="${VERSION:?VERSION is required}"
binary="${RPI_BINARY:?RPI_BINARY is required}"
out="${1:?output zip path is required}"
runtime="$here/node_modules/electron/dist/Electron.app"
license="$here/../../../LICENSE"
[ -d "$runtime" ] || { echo "missing Electron runtime: $runtime" >&2; exit 1; }
[ -f "$binary" ] || { echo "missing release rpi: $binary" >&2; exit 1; }
[ -f "$license" ] || { echo "LICENSE is required in desktop archives" >&2; exit 1; }
[ "$(uname -s)" = "Darwin" ] || { echo "package-mac.sh runs on macOS" >&2; exit 1; }

(cd "$here" && rm -rf dist && npm run build >/dev/null)

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
app="$stage/rpi-desktop.app"
cp -a "$runtime" "$app"
rm -rf "$app/Contents/Resources/default_app.asar"
install -d -m 0755 "$app/Contents/Resources/app"
cp -a "$here/dist" "$app/Contents/Resources/app/dist"
manifest="$(mktemp)"
VERSION="$version" node -e 'const fs=require("fs"); const pkg=require(process.argv[1]); pkg.version=process.env.VERSION; fs.writeFileSync(process.argv[2], JSON.stringify(pkg, null, 2)+"\n");' "$here/package.json" "$manifest"
install -m 0644 "$manifest" "$app/Contents/Resources/app/package.json"
rm -f "$manifest"
install -m 0644 "$here/README.md" "$app/Contents/Resources/app/README.md"
install -m 0755 "$binary" "$app/Contents/Resources/rpi"
install -m 0644 "$license" "$app/Contents/Resources/LICENSE"
if [ -f "$here/../../../NOTICE" ]; then
  install -m 0644 "$here/../../../NOTICE" "$app/Contents/Resources/NOTICE"
fi

plist="$app/Contents/Info.plist"
if /usr/libexec/PlistBuddy -c 'Print :CFBundleName' "$plist" >/dev/null 2>&1; then
  /usr/libexec/PlistBuddy -c 'Set :CFBundleName rpi-desktop' "$plist"
else
  /usr/libexec/PlistBuddy -c 'Add :CFBundleName string rpi-desktop' "$plist"
fi
if /usr/libexec/PlistBuddy -c 'Print :CFBundleDisplayName' "$plist" >/dev/null 2>&1; then
  /usr/libexec/PlistBuddy -c 'Set :CFBundleDisplayName rpi desktop' "$plist"
else
  /usr/libexec/PlistBuddy -c 'Add :CFBundleDisplayName string rpi desktop' "$plist"
fi

case "$out" in
  /*) ;;
  *) out="$PWD/$out" ;;
esac
mkdir -p "$(dirname -- "$out")"
rm -f "$out"
ditto -c -k --keepParent "$app" "$out"
echo "$out"
