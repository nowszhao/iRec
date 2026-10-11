#!/bin/sh
# Builds one .app per architecture and zips both into dist/ for a release.
#
#   ./scripts/package_release.sh
#
# Produces:
#   dist/iRec-<version>-arm64.zip    Apple Silicon
#   dist/iRec-<version>-x86_64.zip   Intel
#   dist/SHA256SUMS
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
export SIGN_IDENTITY="${SIGN_IDENTITY:-}"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
DIST="$PWD/dist"
rm -rf "$DIST"
mkdir -p "$DIST"

for ARCH in arm64 x86_64; do
  STAGE="$DIST/$ARCH"
  ARCH="$ARCH" ./scripts/build_app.sh "$STAGE"
  ./scripts/sign_and_notarize.sh "$STAGE/iRec.app"
  # 打完包先确认能不能过 Gatekeeper：配了 Developer ID 却没公证会直接失败。
  ./scripts/check_signature.sh "$STAGE/iRec.app"
  (cd "$STAGE" && zip -qry "$DIST/iRec-$VERSION-$ARCH.zip" iRec.app)
  rm -rf "$STAGE"
done

(cd "$DIST" && shasum -a 256 iRec-*.zip > SHA256SUMS)

echo "packages in $DIST:"
ls -l "$DIST"
