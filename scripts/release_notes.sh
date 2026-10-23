#!/bin/sh
# 生成 GitHub Release 的正文：CHANGELOG 里 Cargo 版本对应的小节 + 下载安装说明。
#
#   ./scripts/release_notes.sh > dist/RELEASE_BODY.md     # ad-hoc 包
#   SIGNED=1 ./scripts/release_notes.sh > ...             # 已签名公证
#
# 安装文案跟着有没有公证变：公证过的直接双击，没公证的要解除隔离属性。
set -e
cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

# 找到 Cargo.toml 当前版本对应的小节，跳过标题行和开头的空行。
awk -v version="$VERSION" '
  /^## / {
    if (printing) exit
    printing = ($0 == "## " version || index($0, "## " version "（") == 1 || index($0, "## " version " (") == 1)
    next
  }
  printing { print }
' CHANGELOG.md | sed -e '/./,$!d'

printf '\n## 下载\n\n'
printf '| 机型 | 包 |\n| --- | --- |\n'
printf '| Apple Silicon | `iRec-%s-arm64.zip` |\n' "$VERSION"
printf '| Intel | `iRec-%s-x86_64.zip` |\n' "$VERSION"
printf '\n'
if [ -n "$SIGNED" ]; then
  printf '包已用 Developer ID 签名并公证（附票据），解压后直接双击打开；\n'
  printf '首次录制时给「屏幕录制」权限，之后更新不会丢授权（TeamIdentifier 固定）。\n'
else
  printf '包是 ad-hoc 签名、未公证，首次打开会被 Gatekeeper 拦住：\n'
  printf '在 Finder 里右键 → 打开，或执行\n'
  printf '`xattr -dr com.apple.quarantine /Applications/iRec.app`。\n'
  printf '每次换包都是新的代码身份，屏幕录制权限可能要重新授权一次。\n'
fi
printf '\n校验用同目录下的 `SHA256SUMS`。\n'
