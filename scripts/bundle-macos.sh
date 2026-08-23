#!/usr/bin/env bash
# Builds target/bundle/mdview.app — a double-clickable macOS app that also
# registers itself as a viewer for .md/.markdown files.
#
#   scripts/bundle-macos.sh            # build the bundle
#   scripts/bundle-macos.sh --install  # ...and copy it to /Applications
#
# No extra tooling needed beyond cargo and the macOS command-line tools.
#
# Signing: defaults to an ad-hoc signature, which is enough to launch the app.
# However, macOS 15+ silently refuses to make an ad-hoc signed app the default
# handler for .md (Finder's "Change All..." reverts). To allow that, sign with
# a real identity (see `security find-identity -v -p codesigning`):
#
#   MDVIEW_SIGN_IDENTITY="Apple Development: you@example.com (TEAMID)" \
#     scripts/bundle-macos.sh --install
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: this script only builds macOS bundles" >&2
  exit 1
fi

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
APP="target/bundle/mdview.app"

# Build for the machine's native architecture. rustup installed under Rosetta
# defaults to x86_64 even on Apple Silicon, so pin the target explicitly
# (requires `rustup target add aarch64-apple-darwin` on such setups).
case "$(uname -m)" in
  arm64)  TARGET=aarch64-apple-darwin ;;
  x86_64) TARGET=x86_64-apple-darwin ;;
  *) echo "error: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

cargo build --release --locked --target "$TARGET"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$TARGET/release/mdview" "$APP/Contents/MacOS/mdview"
sed "s/__VERSION__/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
if [[ -f packaging/macos/mdview.icns ]]; then
  cp packaging/macos/mdview.icns "$APP/Contents/Resources/mdview.icns"
fi
printf 'APPL????' > "$APP/Contents/PkgInfo"

# A signature is required for the bundle to launch on Apple Silicon. Ad-hoc
# ("-") is enough to run; a real identity (MDVIEW_SIGN_IDENTITY) is needed
# for LaunchServices to accept the app as the default .md handler.
SIGN_IDENTITY="${MDVIEW_SIGN_IDENTITY:--}"
codesign --force --deep --sign "$SIGN_IDENTITY" "$APP" >/dev/null

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
