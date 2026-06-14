#!/bin/bash
#
# weft — 修复 macOS “已损坏，无法打开” / Fix the macOS “is damaged” error
#
# 为什么会这样：weft 的 mac 版还没做 Apple 公证(notarization)，从网上
# 下载后会被 macOS 打上隔离标记(quarantine)，于是报“已损坏”。本脚本只是
# 去掉这个隔离标记，不会改动 app 本身。
#
# Why: the macOS build isn't Apple-notarized yet, so downloads get a
# quarantine flag and macOS reports “is damaged”. This script only removes
# that flag — it does not modify the app.
#
# 用法 / Usage:
#   1. 先把 weft 拖进“应用程序”(Applications)。
#      Drag weft into Applications first.
#   2. 双击本文件。若提示“无法打开”，右键 → 打开 → 打开。
#      Double-click this file. If blocked, right-click → Open → Open.

APP_NAME="weft.app"

echo "──────────────────────────────────────────────"
echo "  weft — 解除 macOS 隔离 / un-quarantine"
echo "──────────────────────────────────────────────"
echo

SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
for TARGET in \
  "/Applications/$APP_NAME" \
  "$HOME/Applications/$APP_NAME" \
  "$SELF_DIR/$APP_NAME" \
  "$HOME/Downloads/$APP_NAME"
do
  [ -d "$TARGET" ] && break
  TARGET=""
done

if [ -z "$TARGET" ]; then
  echo "✗ 没找到 $APP_NAME。请先把它拖进“应用程序”(Applications)，再运行本脚本。"
  echo "✗ Could not find $APP_NAME. Drag it into Applications first, then run again."
  echo
  read -n 1 -s -r -p "按任意键关闭… / Press any key to close…"
  echo
  exit 1
fi

echo "找到 / Found: $TARGET"
echo "正在解除隔离… / removing quarantine…"

# 只删 quarantine 标记，忽略“无此属性”及受保护属性(如 provenance)。
# Remove only the quarantine flag; ignore "no such xattr" and protected attrs.
xattr -dr com.apple.quarantine "$TARGET" 2>/dev/null

# 仍在?可能需要管理员权限。/ Still there? may need admin rights.
if xattr -r "$TARGET" 2>/dev/null | grep -q com.apple.quarantine; then
  echo "需要管理员密码 / admin password required:"
  sudo xattr -dr com.apple.quarantine "$TARGET" 2>/dev/null
fi

# 以“是否还残留 quarantine”判定成败，而非命令退出码。
# Judge by whether quarantine is actually gone, not by exit code.
if xattr -r "$TARGET" 2>/dev/null | grep -q com.apple.quarantine; then
  STATUS=1
else
  STATUS=0
fi

echo
if [ "$STATUS" -eq 0 ]; then
  echo "✓ 完成！现在可以正常双击打开 weft 了。"
  echo "✓ Done — you can open weft normally now."
else
  echo "✗ 失败。可在终端手动运行： xattr -cr \"$TARGET\""
  echo "✗ Failed. Run manually in Terminal: xattr -cr \"$TARGET\""
fi
echo
read -n 1 -s -r -p "按任意键关闭… / Press any key to close…"
echo
