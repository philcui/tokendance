#!/usr/bin/env bash
# 推送前的自检：确认这一份里没有混进"只该存在于本机"的东西，并且它真的能构建。
#
#   ./scripts/privacy_check.sh
#
# 三件事：① 扫个人/运营痕迹；② 扫密钥形态；③ 用这份源码真构建一次。
set -euo pipefail
cd "$(cd "$(dirname "$0")/.." && pwd)"

# **只扫"会被提交的东西"**：git 跟踪的 + 未跟踪但没被忽略的。
#
# 之前这里是 `grep -r .`，它不看 .gitignore，于是把一个**永远不会被推上去**的本机文件也算进来：
# `rust-server/.cargo/config.toml` 里写着 `/Users/<你>/Library/Caches/tokendance-vendor`，
# 结果自检永远失败（2026-09-18 实测踩到）。判据应该和"push 会送什么"完全一致，而不是和
# "工作区里有什么"一致。
# 排除它自己：下面那份模式名单本身就是这些字符串，扫自己必然命中。（以前靠 grep 的
# `--exclude`，但那个只对递归搜索生效，文件清单是自己拼的时候不生效，所以在这里过滤。）
list0() {
  git ls-files -z --cached --others --exclude-standard \
    | while IFS= read -r -d '' f; do [ "$f" = "scripts/privacy_check.sh" ] || printf '%s\0' "$f"; done
}
paths() { list0 | tr '\0' '\n'; }
# -I 跳过二进制（build/ 里的图、vendor 的 js 之类），免得刷屏
scan() { list0 | xargs -0 grep -nIE "$@" 2>/dev/null || true; }

echo "==> ① 个人 / 运营痕迹（这些不该出现在公开仓库里）"
bad=0

# a) 当前账号名的绝对路径——真正要防的就是这个（测试夹具里的 /Users/x 是占位符，不算）
ME="${USER:-$(id -un)}"
hits="$(scan -- "/Users/$ME")"
if [ -n "$hits" ]; then echo "  ✗ 出现你的家目录路径 (/Users/$ME)"; echo "$hits" | head -5 | sed 's/^/      /'; bad=1; fi

# b) 其它人的 /Users/<名字>——占位符放行，别的一律拦
hits="$(scan "/Users/[a-zA-Z][a-zA-Z0-9_.-]*" \
        | grep -vE "/Users/(x|alice|bob|user|you|me|example|test|someone|u)\b" || true)"
if [ -n "$hits" ]; then echo "  ✗ 出现别人的家目录路径（若确是占位符，加进上面那行的放行名单）"; echo "$hits" | head -5 | sed 's/^/      /'; bad=1; fi

# c) 服务器 / 密钥 / 凭据形态
for pat in "115\.29\.196\.[0-9]+" "\.ssh/" "fanshitou_ecs" "ADMIN_TOKEN=[0-9a-f]{16}" "ADMIN_PASSWORD=[^c]"; do
  hits="$(scan -- "$pat")"
  if [ -n "$hits" ]; then echo "  ✗ /$pat/"; echo "$hits" | head -5 | sed 's/^/      /'; bad=1; fi
done
[ "$bad" = 0 ] && echo "  ✓ 干净（家目录路径只允许占位符，服务器/密钥痕迹一处没有）"

# d) 页面不许引外链脚本/样式。仪表盘曾经从 cdn.jsdelivr.net 拉 chart.js：那等于每次
#    打开都把用户 IP 送给第三方，还让别人的代码进了能读整个本地库的页面。要加库就
#    放进 vendor/（见 vendor/README.md），不要退回 CDN。
hits="$(paths | grep -E '\.html$' | xargs grep -nIE '(<script|<link)[^>]*(src|href)="https?://' 2>/dev/null || true)"
if [ -n "$hits" ]; then echo "  ✗ 页面引用了外部资源（应改成 /vendor/…）"; echo "$hits" | head -5 | sed 's/^/      /'; bad=1; fi
[ -n "$hits" ] || echo "  ✓ 四个页面没有任何外链脚本/样式（只有本机 + 你的域名）"

echo "==> ② 远端地址集中在一处（应该只有 ServiceConfig.swift.in 与 README；vendor/ 里的许可注释不算）"
list0 | xargs -0 grep -lI "https://" 2>/dev/null | sed 's/^/      /' || true

if [ "$bad" != 0 ]; then echo "!! 自检没过，别推" >&2; exit 1; fi

echo "==> ③ 用这份源码构建（证明它自足）"
./scripts/build_app.sh >/tmp/privacy-check-build.log 2>&1 \
  && echo "  ✓ 构建成功（产物在 build/，不在 git 里）" \
  || { echo "  ✗ 构建失败，见 /tmp/privacy-check-build.log"; tail -5 /tmp/privacy-check-build.log; exit 1; }
