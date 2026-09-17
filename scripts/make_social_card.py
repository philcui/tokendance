#!/usr/bin/env python3
"""生成 GitHub 的社交预览图（分享到 X / Slack / 微信时显示的那张 1280×640 卡片）。

    python3 scripts/make_social_card.py            # → docs/social-preview.png
    python3 scripts/make_social_card.py --zh       # → docs/social-preview.zh-CN.png

为什么值得单独做个脚本：GitHub 的社交卡片是**手工上传**的（Settings → General → Social
preview），不是从 README 自动生成的。它一旦过期（比如界面换了、版本号变了），别人转发出去
的那张图就是旧样子，而且没人会想起来改。放在仓库里、有脚本可重跑，就不会烂在那儿。

素材全部来自仓库里已经有的东西：`Assets/icon_1024.png` 与 `docs/hud.gif` 的第一帧——后者是
真机录的挂件，也就是这个产品最能一眼看懂的那部分。
"""
import argparse
import pathlib
import sys

from PIL import Image, ImageDraw, ImageFont

W, H = 1280, 640
ROOT = pathlib.Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"
SF = "/System/Library/Fonts/SFNS.ttf"
PINGFANG = "/System/Library/Fonts/PingFang.ttc"


def font(path, size, index=0, weight=None):
    f = ImageFont.truetype(path, size, index=index)
    if weight:                      # SFNS 是可变字体，Pillow 支持按名字取字重
        try:
            f.set_variation_by_name(weight)
        except Exception:
            pass
    return f


def gradient(w, h, top, bottom):
    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    for y in range(h):
        t = y / max(1, h - 1)
        d.line([(0, y), (w, y)],
               fill=tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)))
    return img


def rounded(img, radius):
    mask = Image.new("L", img.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, img.size[0] - 1, img.size[1] - 1],
                                           radius=radius, fill=255)
    out = img.convert("RGBA")
    out.putalpha(mask)
    return out


def shadow(size, radius, blur, alpha):
    pad = blur * 2
    layer = Image.new("RGBA", (size[0] + pad * 2, size[1] + pad * 2), (0, 0, 0, 0))
    ImageDraw.Draw(layer).rounded_rectangle(
        [pad, pad, pad + size[0] - 1, pad + size[1] - 1], radius=radius, fill=(0, 0, 0, alpha))
    return layer.filter(__import__("PIL.ImageFilter", fromlist=["GaussianBlur"]).GaussianBlur(blur)), pad


def build(zh: bool) -> Image.Image:
    card = gradient(W, H, (13, 17, 23), (22, 27, 34)).convert("RGBA")

    # 挂件：真机录的，缩放后贴右侧，带圆角与柔和阴影
    hud = Image.open(DOCS / "hud.gif").convert("RGB")
    hud = hud.resize((560, round(hud.height * 560 / hud.width)), Image.LANCZOS)
    hud = rounded(hud, 34)
    sh, pad = shadow(hud.size, 34, 26, 150)
    hx, hy = 660, (H - hud.height) // 2
    card.alpha_composite(sh, (hx - pad, hy - pad))
    card.alpha_composite(hud, (hx, hy))

    d = ImageDraw.Draw(card)
    x = 88

    icon = Image.open(ROOT / "Assets" / "icon_1024.png").convert("RGBA").resize((64, 64), Image.LANCZOS)
    card.alpha_composite(icon, (x, 92))

    if zh:
        title = font(PINGFANG, 68, index=5)          # PingFang SC Medium
        tag = font(PINGFANG, 27, index=5)
        small = font(PINGFANG, 19, index=2)
        lines = ["AI 编码 agent 烧掉的 token，", "变成菜单栏上一个随时能看的数字。"]
        foot = "数据只在本机解析 · 没有账号 · 不用登录"
    else:
        title = font(SF, 68, weight="Bold")
        tag = font(SF, 26, weight="Regular")
        small = font(SF, 18, weight="Regular")
        lines = ["The token your AI coding agents burn,", "live in your menu bar."]
        foot = "Parsed and stored locally · no account · no login"

    d.text((x + 80, 96), "TokenDance", font=title, fill=(240, 244, 248))

    y = 214
    for ln in lines:
        d.text((x, y), ln, font=tag, fill=(154, 164, 178))
        y += 40
    d.text((x, y + 26), foot, font=small, fill=(110, 120, 134))

    agents = "Codex · Claude Code ·" if not zh else "Codex · Claude Code ·"
    more = "and anything else you have installed" if not zh else "以及你自己装的其它工具"
    d.text((x, y + 62), f"{agents} {more}", font=small, fill=(110, 120, 134))
    return card.convert("RGB")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--zh", action="store_true", help="生成中文卡片")
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    out = pathlib.Path(a.out) if a.out else DOCS / ("social-preview.zh-CN.png" if a.zh else "social-preview.png")
    img = build(a.zh)
    img.save(out, optimize=True)
    print(f"{out}  {img.size[0]}x{img.size[1]}  {out.stat().st_size / 1024:.0f} KB")


if __name__ == "__main__":
    sys.exit(main())
