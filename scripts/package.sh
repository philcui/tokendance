#!/bin/bash
# Turn build/TokenDance.app into the archive the release pipeline expects.
#
#   ./scripts/package.sh              # → build/TokenDance-<version>-mac.zip
#   ./scripts/package.sh --dmg        # → and build/TokenDance-<version>.dmg
#   ./scripts/package.sh --no-build   # skip build_app.sh (only if you just built)
#
# Three things are easy to get wrong here, and all three have been seen:
#
#   * **The name is not up to us.** The backend stores a release as
#     `TokenDance-<version>-mac.zip` (`release_name()` in backend/src/release.rs)
#     and `Updater.swift` asks for exactly that file, unpacks it and looks for
#     `TokenDance.app` **at the root of the archive**. A zip whose root is a
#     folder, or one named `tokendance.zip`, downloads fine and then fails on
#     some user's machine.
#   * **`zip` is the wrong tool.** It drops symlinks and resource forks, and adds
#     `__MACOSX`. `ditto -c -k --sequesterRsrc --keepParent` is the macOS-native
#     way and produces the layout the updater expects.
#   * **A stale bundle is worse than no bundle.** This repo has already shipped a
#     previous build once, so the default here is to build first rather than
#     trust whatever is sitting in build/.
set -euo pipefail
cd "$(cd "$(dirname "$0")/.." && pwd)"

APP="build/TokenDance.app"
DO_BUILD=1
DO_DMG=0
for a in "$@"; do
  case "$a" in
    --no-build) DO_BUILD=0 ;;
    --dmg) DO_DMG=1 ;;
    *) echo "用法: $0 [--dmg] [--no-build]" >&2; exit 2 ;;
  esac
done

[ "$DO_BUILD" = 1 ] && ./scripts/build_app.sh
[ -d "$APP" ] || { echo "!! 没有 $APP，先跑 ./scripts/build_app.sh" >&2; exit 1; }

# The version comes from the thing being shipped, never from a variable here:
# a zip named 1.3.0 containing a 1.3.1 app makes the updater offer an update
# forever, and nobody notices until users complain about the popup.
VER=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$APP/Contents/Info.plist")
BUNDLE=$(/usr/libexec/PlistBuddy -c "Print :CFBundleIdentifier" "$APP/Contents/Info.plist")
ZIP="build/TokenDance-${VER}-mac.zip"
BIN="$APP/Contents/MacOS/TokenUsage"
echo "==> 打包 $APP  ($BUNDLE $VER)"

rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"

# Round-trip it. Unpacking is the only way to know the root layout is right, and
# the layout is the thing the updater depends on.
TMP="$(mktemp -d "${TMPDIR:-/tmp}/td-pkg-XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
ditto -x -k "$ZIP" "$TMP"
[ -d "$TMP/TokenDance.app" ] || { echo "!! 解压后根目录里没有 TokenDance.app —— 更新器会拒绝安装" >&2; exit 1; }

if [ -f "$TMP/TokenDance.app/Contents/MacOS/TokenUsage" ]; then
  A=$(shasum -a 256 "$BIN" | awk '{print $1}')
  B=$(shasum -a 256 "$TMP/TokenDance.app/Contents/MacOS/TokenUsage" | awk '{print $1}')
  [ "$A" = "$B" ] || { echo "!! 解压出来的主程序和原包不一致" >&2; exit 1; }
  echo "    ✓ 根目录有 TokenDance.app，主程序 sha256 一致"
else
  echo "!! 解压后找不到主程序" >&2; exit 1
fi

if codesign --verify --deep --strict "$TMP/TokenDance.app" 2>/dev/null; then
  echo "    ✓ 签名自洽（codesign --verify --deep --strict）"
else
  echo "    !! 签名校验失败" >&2; exit 1
fi

# Reported, not enforced: this is expected to be "rejected" until the build is
# signed with a Developer ID and notarised. See deploy/LAUNCH.md.
spctl -a -vvv -t exec "$TMP/TokenDance.app" >/dev/null 2>&1 \
  && echo "    ✓ Gatekeeper 接受（已签名 + 已公证）" \
  || echo "    · Gatekeeper 会拦（ad-hoc 签名，未公证）—— 用户要右键打开一次，见 README"

SIZE=$(du -h "$ZIP" | cut -f1)
SHA=$(shasum -a 256 "$ZIP" | awk '{print $1}')
echo ""
echo "    $ZIP"
echo "    大小 $SIZE"
echo "    sha256 $SHA"

if [ "$DO_DMG" = 1 ]; then
  DMG="build/TokenDance-${VER}.dmg"
  STAGE="$TMP/dmg"
  mkdir -p "$STAGE"
  cp -R "$APP" "$STAGE/"
  ln -s /Applications "$STAGE/Applications"       # 拖进去就能装
  rm -f "$DMG"
  hdiutil create -quiet -volname "TokenDance" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
  echo ""
  echo "    $DMG"
  echo "    大小 $(du -h "$DMG" | cut -f1)"
  echo "    注意：DMG 给**人**用（拖拽安装）；app 内更新器只认那个 zip。"
fi

echo ""
echo "发版：后台「发版」页上传这个 zip，版本填 $VER（SHA 页面会自己算）。"
