#!/bin/bash
#
# Regenerate src-tauri/dmg/background.tiff from background.svg.
#
# The DMG window is 660x520. The SVG is authored on a 660x660 square with the
# design vertically centered, so a centered crop recovers the window region
# exactly (sips only crops from the center). Uses built-in macOS tools only —
# qlmanage renders SVG via WebKit, so CJK text uses the system font.
#
# Run after editing background.svg, then commit the regenerated .tiff.
set -euo pipefail
cd "$(dirname "$0")/.."
DIR="src-tauri/dmg"
W=660
H=520
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# @2x rep
qlmanage -t -s "$((W * 2))" -o "$tmp" "$DIR/background.svg" >/dev/null
sips -c "$((H * 2))" "$((W * 2))" "$tmp/background.svg.png" --out "$tmp/bg2x.png" >/dev/null

# @1x rep
qlmanage -t -s "$W" -o "$tmp" "$DIR/background.svg" >/dev/null
sips -c "$H" "$W" "$tmp/background.svg.png" --out "$tmp/bg1x.png" >/dev/null

# Combine into a single HiDPI multi-rep TIFF (1x base + 2x).
tiffutil -cathidpicheck "$tmp/bg1x.png" "$tmp/bg2x.png" -out "$DIR/background.tiff" >/dev/null
echo "wrote $DIR/background.tiff (${W}x${H} @1x + $((W * 2))x$((H * 2)) @2x)"
