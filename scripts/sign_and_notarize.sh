#!/bin/sh
# Signs iRec.app and notarizes it when credentials are available.
#
#   SIGN_IDENTITY="Developer ID Application: Name (TEAMID)"   # 签名（有则走 hardened runtime）
#   公证二选一：
#     APPLE_ID + APPLE_TEAM_ID + APPLE_APP_PASSWORD           # App 专用密码
#     APPLE_API_KEY_ID + APPLE_API_ISSUER + APPLE_API_PRIVATE_KEY  # App Store Connect API key（base64 的 .p8）
#
# 没有任何凭据时退化为 ad-hoc 签名，本地构建不受影响。
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

APP="$1"
[ -n "$APP" ] || { echo "usage: $0 <path-to-iRec.app>" >&2; exit 1; }

if [ -n "$SIGN_IDENTITY" ]; then
  codesign --force --deep --timestamp --options runtime \
    --entitlements resources/iRec.entitlements \
    --sign "$SIGN_IDENTITY" "$APP"
  echo "signed: $SIGN_IDENTITY (hardened runtime + timestamp)"
else
  codesign --force --deep --sign - "$APP"
  echo "signed: ad-hoc"
fi

# 公证要求 Developer ID 签名；没有证书或 Apple 凭据就跳过。
if [ -z "$SIGN_IDENTITY" ]; then
  echo "notarization: skipped (no SIGN_IDENTITY)"
  exit 0
fi

STAGE="$(dirname "$APP")"
ZIP="$STAGE/$(basename "$APP" .app)-notarize.zip"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"

if [ -n "$APPLE_API_KEY_ID" ] && [ -n "$APPLE_API_ISSUER" ] && [ -n "$APPLE_API_PRIVATE_KEY" ]; then
  mkdir -p "$HOME/.private_keys"
  printf '%s' "$APPLE_API_PRIVATE_KEY" | base64 --decode >"$HOME/.private_keys/AuthKey_$APPLE_API_KEY_ID.p8"
  xcrun notarytool submit "$ZIP" \
    --key "$HOME/.private_keys/AuthKey_$APPLE_API_KEY_ID.p8" \
    --key-id "$APPLE_API_KEY_ID" \
    --issuer "$APPLE_API_ISSUER" \
    --wait
elif [ -n "$APPLE_ID" ] && [ -n "$APPLE_TEAM_ID" ] && [ -n "$APPLE_APP_PASSWORD" ]; then
  xcrun notarytool submit "$ZIP" \
    --apple-id "$APPLE_ID" \
    --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_APP_PASSWORD" \
    --wait
else
  rm -f "$ZIP"
  echo "notarization: skipped (no Apple credentials)"
  exit 0
fi

xcrun stapler staple "$APP"
rm -f "$ZIP"
echo "notarized and stapled"
