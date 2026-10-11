#!/bin/sh
# 看看这个 .app 能不能过 Gatekeeper。
#
#   ./scripts/check_signature.sh dist/arm64/iRec.app
#
# ad-hoc 签名（没配证书）会被 Gatekeeper 拒绝，这里只提示、不算失败；
# 配了 Developer ID 还被拒，说明公证没成功，直接报错让 CI 停下来。
set -e
APP="$1"
[ -n "$APP" ] || { echo "usage: $0 <path-to-iRec.app>" >&2; exit 1; }

echo "== codesign =="
codesign -vv --deep --strict "$APP" 2>&1 | tail -2

echo "== Gatekeeper（spctl）=="
# 不能直接用 `if spctl | tail`：管道的退出码是 tail 的，被拒也会当成成功。
OUT="$(spctl -a -vvv "$APP" 2>&1 || true)"
printf '%s\n' "$OUT" | tail -3
if printf '%s\n' "$OUT" | grep -q "accepted"; then
  echo "结论：允许打开"
else
  echo "结论：被 Gatekeeper 拦下"
  if [ -n "$SIGN_IDENTITY" ]; then
    echo "配了 Developer ID 却被拒：公证这一步没成功，看一下上面的 notarytool 输出" >&2
    exit 1
  fi
  echo "ad-hoc 签名、未公证：装的时候需要右键打开或清掉隔离属性"
fi

if [ -n "$SIGN_IDENTITY" ]; then
  echo "== 公证票据 =="
  xcrun stapler validate "$APP" 2>&1 | tail -2 || true
fi
