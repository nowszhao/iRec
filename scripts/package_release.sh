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

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
DIST="$PWD/dist"
rm -rf "$DIST"
mkdir -p "$DIST"

for ARCH in arm64 x86_64; do
  STAGE="$DIST/$ARCH"
  ARCH="$ARCH" ./scripts/build_app.sh "$STAGE"
  ./scripts/sign_and_notarize.sh "$STAGE/iRec.app"
  (cd "$STAGE" && zip -qry "$DIST/iRec-$VERSION-$ARCH.zip" iRec.app)
  rm -rf "$STAGE"
done

(cd "$DIST" && shasum -a 256 iRec-*.zip > SHA256SUMS)

echo "packages in $DIST:"
ls -l "$DIST"
