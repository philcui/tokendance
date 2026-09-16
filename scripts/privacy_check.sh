#!/usr/bin/env bash
# 推送前的自检：确认这一份里没有混进"只该存在于本机"的东西，并且它真的能构建。
#
#   ./scripts/privacy_check.sh
#
# 三件事：① 扫个人/运营痕迹；② 扫密钥形态；③ 用这份源码真构建一次。
set -euo pipefail
cd "$(cd "$(dirname "$0")/.." && pwd)"

echo "==> ① 个人 / 运营痕迹（这些不该出现在公开仓库里）"
PATTERNS=(
  "/Users/[a-zA-Z]"          # 绝对家目录路径
  "115\.29\.196\.[0-9]+"     # 某台服务器的 IP
  "\.ssh/"                   # 私钥路径
  "fanshitou_ecs"
  "ADMIN_TOKEN=[0-9a-f]{16}"
  "ADMIN_PASSWORD=[^c]"      # 明文口令（占位符是 change-me… 也一样要改）
)
bad=0
for pat in "${PATTERNS[@]}"; do
  hits="$(grep -rnE --exclude-dir=.git --exclude-dir=target --exclude-dir=build "$pat" . 2>/dev/null || true)"
  if [ -n "$hits" ]; then echo "  ✗ /$pat/"; echo "$hits" | head -5 | sed 's/^/      /'; bad=1; fi
done
[ "$bad" = 0 ] && echo "  ✓ 干净"

echo "==> ② 远端地址集中在一处（应该只有 ServiceConfig.swift.in 与 README）"
grep -rln "https://" --exclude-dir=.git --exclude-dir=target --exclude-dir=build . \
  | sed 's/^/      /'

if [ "$bad" != 0 ]; then echo "!! 自检没过，别推" >&2; exit 1; fi

echo "==> ③ 用这份源码构建（证明它自足）"
./scripts/build_app.sh >/tmp/privacy-check-build.log 2>&1 \
  && echo "  ✓ 构建成功（产物在 build/，不在 git 里）" \
  || { echo "  ✗ 构建失败，见 /tmp/privacy-check-build.log"; tail -5 /tmp/privacy-check-build.log; exit 1; }
