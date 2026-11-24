#!/usr/bin/env bash
# 用系统自带 hdiutil 把已打包的 .app 制成可分发的 .dmg。
# 不依赖 create-dmg，避免其模板资源缺失导致 `tauri build` 的 dmg 步骤失败。
# 用法：先 `npm run tauri build`（生成 .app），再 `bash scripts/build-dmg.sh`
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
APP_DIR="$SCRIPT_DIR/src-tauri/target/release/bundle/macos"
APP="$(find "$APP_DIR" -maxdepth 1 -name '*.app' | head -1)"
if [ -z "$APP" ]; then
  echo "未找到 .app，请先运行: npm run tauri build" >&2
  exit 1
fi

NAME="$(basename "$APP" .app)"
OUT_DIR="$SCRIPT_DIR/src-tauri/target/release/bundle/dmg"
DMG="$OUT_DIR/${NAME}_manual.dmg"
TMP="${DMG%.dmg}.rw.dmg"
mkdir -p "$OUT_DIR"
rm -f "$TMP" "$DMG"

SIZE_MB=$(( $(du -sm "$APP" | cut -f1) + 24 ))
echo "创建可写镜像 (${SIZE_MB}MB)…"
hdiutil create -srcfolder "$APP" -volname "$NAME" -fs HFS+ -format UDRW -size ${SIZE_MB}m "$TMP"

DEV="$(hdiutil attach -readwrite -noverify -noautoopen "$TMP" | grep '^/dev/' | head -1 | awk '{print $1}')"
MNT="/Volumes/$NAME"
echo "挂载于 $MNT ($DEV)"
# 提供“拖到应用程序”快捷方式（非必须，纯便利）
ln -s /Applications "$MNT/Applications" 2>/dev/null || true
sleep 2

echo "卸载镜像…"
hdiutil detach "$DEV" || hdiutil detach "$DEV" -force || true

echo "压缩为最终 dmg…"
hdiutil convert "$TMP" -format UDZO -imagekey zlib-level=9 -o "$DMG"
rm -f "$TMP"
echo "已生成: $DMG"
