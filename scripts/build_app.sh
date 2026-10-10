#!/bin/sh
# Builds iRec.app into ../outputs (release binary + app bundle).
#
# Signs with a stable "Apple Development" identity when one is available, so
# macOS keeps remembering the screen recording permission between rebuilds.
# Falls back to an ad-hoc signature otherwise.
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

cargo build --release

OUT="${1:-../outputs}"
APP="$OUT/iRec.app"
# Replace any previous copy: `cp -R` would otherwise nest the new bundle inside
# the old one and the stale app keeps running.
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/iRec "$APP/Contents/MacOS/iRec"
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

echo "built: $(cd "$OUT" && pwd)/iRec.app"
