#!/usr/bin/env bash
# 推送前的自检：确认这一份里没有混进"只该存在于本机"的东西，并且它真的能构建。
#
#   ./scripts/privacy_check.sh
#
# 三件事：① 扫个人/运营痕迹；② 扫密钥形态；③ 用这份源码真构建一次。
set -euo pipefail
cd "$(cd "$(dirname "$0")/.." && pwd)"

echo "==> ① 个人 / 运营痕迹（这些不该出现在公开仓库里）"
bad=0
excl=(--exclude-dir=.git --exclude-dir=target --exclude-dir=build --exclude=privacy_check.sh)

# a) 当前账号名的绝对路径——真正要防的就是这个（测试夹具里的 /Users/x 是占位符，不算）
ME="${USER:-$(id -un)}"
hits="$(grep -rn --exclude-dir=.git --exclude-dir=target --exclude-dir=build --exclude=privacy_check.sh -- "/Users/$ME" . 2>/dev/null || true)"
if [ -n "$hits" ]; then echo "  ✗ 出现你的家目录路径 (/Users/$ME)"; echo "$hits" | head -5 | sed 's/^/      /'; bad=1; fi

# b) 其它人的 /Users/<名字>——占位符放行，别的一律拦
hits="$(grep -rnE --exclude-dir=.git --exclude-dir=target --exclude-dir=build --exclude=privacy_check.sh "/Users/[a-zA-Z][a-zA-Z0-9_.-]*" . 2>/dev/null \
        | grep -vE "/Users/(x|alice|bob|user|you|me|example|test|someone|u)\b" || true)"
if [ -n "$hits" ]; then echo "  ✗ 出现别人的家目录路径（若确是占位符，加进上面那行的放行名单）"; echo "$hits" | head -5 | sed 's/^/      /'; bad=1; fi

# c) 服务器 / 密钥 / 凭据形态
for pat in "115\.29\.196\.[0-9]+" "\.ssh/" "fanshitou_ecs" "ADMIN_TOKEN=[0-9a-f]{16}" "ADMIN_PASSWORD=[^c]"; do
  hits="$(grep -rnE "${excl[@]}" -- "$pat" . 2>/dev/null || true)"
  if [ -n "$hits" ]; then echo "  ✗ /$pat/"; echo "$hits" | head -5 | sed 's/^/      /'; bad=1; fi
done
[ "$bad" = 0 ] && echo "  ✓ 干净（家目录路径只允许占位符，服务器/密钥痕迹一处没有）"

echo "==> ② 远端地址集中在一处（应该只有 ServiceConfig.swift.in 与 README）"
grep -rln "https://" --exclude-dir=.git --exclude-dir=target --exclude-dir=build . \
  | sed 's/^/      /'

if [ "$bad" != 0 ]; then echo "!! 自检没过，别推" >&2; exit 1; fi

echo "==> ③ 用这份源码构建（证明它自足）"
./scripts/build_app.sh >/tmp/privacy-check-build.log 2>&1 \
  && echo "  ✓ 构建成功（产物在 build/，不在 git 里）" \
  || { echo "  ✗ 构建失败，见 /tmp/privacy-check-build.log"; tail -5 /tmp/privacy-check-build.log; exit 1; }
