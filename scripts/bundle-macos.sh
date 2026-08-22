#!/usr/bin/env bash
# Builds target/bundle/mdview.app — a double-clickable macOS app that also
# registers itself as a viewer for .md/.markdown files.
#
#   scripts/bundle-macos.sh            # build the bundle
#   scripts/bundle-macos.sh --install  # ...and copy it to /Applications
#
# No extra tooling needed beyond cargo and the macOS command-line tools.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: this script only builds macOS bundles" >&2
  exit 1
fi

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
APP="target/bundle/mdview.app"

cargo build --release --locked

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/mdview "$APP/Contents/MacOS/mdview"
sed "s/__VERSION__/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
if [[ -f packaging/macos/mdview.icns ]]; then
  cp packaging/macos/mdview.icns "$APP/Contents/Resources/mdview.icns"
fi
printf 'APPL????' > "$APP/Contents/PkgInfo"

# Ad-hoc signature: required for the bundle to launch on Apple Silicon and
# enough for a locally built app (no Developer ID / notarization involved).
codesign --force --deep --sign - "$APP" >/dev/null

echo "built $APP (v$VERSION)"

if [[ "${1:-}" == "--install" ]]; then
  DEST="/Applications/mdview.app"
  rm -rf "$DEST"
  cp -R "$APP" "$DEST"
  # Let LaunchServices pick up the installed bundle and its .md association,
  # and forget the build copy under target/ so `open -a mdview` (and Finder's
  # "Open With") resolve to /Applications rather than a stale build.
  LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister
  "$LSREGISTER" -u "$ROOT/$APP" >/dev/null 2>&1 || true
  "$LSREGISTER" -f "$DEST" >/dev/null 2>&1 || true
  echo "installed $DEST"
fi
