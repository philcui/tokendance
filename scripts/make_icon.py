#!/usr/bin/env python3
"""TokenDance app icon v2 — engine cylinder + piston, single-hue amber.
Geometry source of truth; mirrors Assets/icon.svg. 4x supersampled.
Output: Assets/icon_1024.png"""
from PIL import Image, ImageDraw, ImageFilter

S = 4
BASE = 1024
W = BASE * S

def sc(v): return v * S

def vgrad(w, h, stops):
    col = Image.new("RGB", (1, h))
    px = col.load()
    for y in range(h):
        t = y / (h - 1)
        for i in range(len(stops) - 1):
            p0, c0 = stops[i]; p1, c1 = stops[i + 1]
            if p0 <= t <= p1:
                f = (t - p0) / (p1 - p0) if p1 > p0 else 0
                px[0, y] = tuple(int(a + (b - a) * f) for a, b in zip(c0, c1))
                break
    return col.resize((w, h))

img = Image.new("RGBA", (W, W), (0, 0, 0, 0))

# ---------- squircle background (neutral dark, lets colored blocks pop) ----------
squircle = Image.new("L", (W, W), 0)
ImageDraw.Draw(squircle).rounded_rectangle([sc(64), sc(64), sc(960), sc(960)], radius=sc(200), fill=255)
bg = vgrad(W, W, [(0.0, (28, 23, 34)), (1.0, (16, 13, 20))]).convert("RGBA")
img.paste(bg, (0, 0), squircle)

# faint center lift (neutral)
rg = Image.radial_gradient("L").resize((sc(840), sc(840))).point(lambda v: int((255 - v) * 0.08))
glow = Image.new("RGBA", (W, W), (0, 0, 0, 0))
glow.paste(Image.new("RGBA", (sc(840), sc(840)), (200, 190, 255, 255)), (sc(92), sc(140)), rg)
img = Image.alpha_composite(img, Image.composite(glow, Image.new("RGBA", (W, W), (0, 0, 0, 0)), squircle))

# hairline rim for depth
rim = Image.new("RGBA", (W, W), (0, 0, 0, 0))
ImageDraw.Draw(rim).rounded_rectangle([sc(66), sc(66), sc(958), sc(958)], radius=sc(198), outline=(255, 255, 255, 30), width=sc(3))
img = Image.alpha_composite(img, Image.composite(rim, Image.new("RGBA", (W, W), (0, 0, 0, 0)), squircle))

# ---------- glyph: rhythm-game note blocks, bouncing ----------
# 4 bigger blocks, each its own agent color; top row and bottom row
# strictly aligned, wide vertical spread.
BLOCKS = [
    # (cx, cy, light, base) — colors mirror the app's agent palette
    (320, 448, (126, 179, 255), (47, 123, 246)),   # blue
    (448, 576, (255, 192, 138), (249, 115, 22)),   # orange
    (576, 448, (140, 235, 165), (34, 197, 94)),    # green
    (704, 576, (201, 180, 255), (139, 92, 246)),   # purple
]
BS, RX = 120, 32

def vgrad_rgb(w, h, c_top, c_bot):
    return vgrad(w, h, [(0.0, c_top), (1.0, c_bot)])

for cx, cy, c_top, c_base in BLOCKS:
    m = Image.new("L", (W, W), 0)
    ImageDraw.Draw(m).rounded_rectangle(
        [sc(cx - BS // 2), sc(cy - BS // 2), sc(cx + BS // 2), sc(cy + BS // 2)],
        radius=sc(RX), fill=255)
    # same-hue neon glow
    gm = m.filter(ImageFilter.GaussianBlur(sc(28)))
    gc = Image.new("RGBA", (W, W), c_base + (255,))
    gc.putalpha(gm.point(lambda v: int(v * 0.5)))
    img = Image.alpha_composite(img, Image.composite(gc, Image.new("RGBA", (W, W), (0, 0, 0, 0)), squircle))
    # crisp block
    blk = vgrad_rgb(W, W, c_top, c_base).convert("RGBA")
    img.paste(blk, (0, 0), Image.composite(m, Image.new("L", (W, W), 0), squircle))

img = img.resize((BASE, BASE), Image.LANCZOS)
img.save("Assets/icon_1024.png")
print("saved Assets/icon_1024.png (v2 cylinder/piston)")
