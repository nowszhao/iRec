#!/bin/sh
# Builds iRec.app (release binary + app bundle).
#
#   ./scripts/build_app.sh [output_dir]        # host architecture
#   ARCH=arm64    ./scripts/build_app.sh       # Apple Silicon
#   ARCH=x86_64   ./scripts/build_app.sh       # Intel (cross-compiled)
#   ARCH=universal ./scripts/build_app.sh      # both, merged with lipo
#
# Signs with the identity given in SIGN_IDENTITY when it is set (keeps the
# system-granted recording permission across rebuilds), ad-hoc otherwise.
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

HOST_ARCH="$(uname -m)"
ARCH="${ARCH:-$HOST_ARCH}"
OUT="${1:-target/release}"

case "$ARCH" in
  arm64) TARGET=aarch64-apple-darwin ;;
  x86_64) TARGET=x86_64-apple-darwin ;;
  universal) TARGET=universal ;;
  *)
    echo "unknown ARCH=$ARCH (use arm64, x86_64 or universal)" >&2
    exit 1
    ;;
esac

build_target() {
  rustup target add "$1" >/dev/null 2>&1 || true
  cargo build --release --target "$1"
}

if [ "$TARGET" = "universal" ]; then
  build_target aarch64-apple-darwin
  build_target x86_64-apple-darwin
  mkdir -p target/universal/release
  lipo -create \
    target/aarch64-apple-darwin/release/iRec \
    target/x86_64-apple-darwin/release/iRec \
    -output target/universal/release/iRec
  BIN=target/universal/release/iRec
else
  build_target "$TARGET"
  BIN="target/$TARGET/release/iRec"
fi

APP="$OUT/iRec.app"
# Replace any previous copy: `cp -R` would otherwise nest the new bundle inside
# the old one and the stale app keeps running.
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/iRec"
cp resources/Info.plist "$APP/Contents/Info.plist"
cp resources/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
chmod +x "$APP/Contents/MacOS/iRec"

# Use the SHA-1 of the identity: several certs can share the same name.
# Ad-hoc signing by default: development certificates without a provisioning
# profile can make macOS refuse to show the screen recording prompt. Set
# SIGN_IDENTITY to use a real certificate (and then avoid rebuilding often).
if [ -n "$SIGN_IDENTITY" ]; then
  codesign --force --deep --sign "$SIGN_IDENTITY" "$APP"
  echo "signed with: $SIGN_IDENTITY"
else
  codesign --force --deep --sign - "$APP"
  echo "signed ad-hoc"
fi

echo "built ($ARCH): $(cd "$OUT" && pwd)/iRec.app"
