# -*- coding: utf-8 -*-
"""生成 rust-seek 图标资源（可重复运行）。

产物：
  assets/rust-seek.ico       多尺寸 Windows 图标（exe 资源 / 托盘共用）
  assets/rust-seek-64.png    64px RGBA（托盘 & 窗口图标运行时解码）
  assets/rust-seek-256.png   256px 预览图

依赖：Pillow（pip install pillow）
"""
import math
from pathlib import Path

from PIL import Image, ImageDraw

S = 512
here = Path(__file__).resolve().parent

img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# 深蓝灰圆角底 + 顶部微高光
d.rounded_rectangle([16, 16, S - 16, S - 16], radius=100, fill=(30, 41, 59, 255))
hl = Image.new("RGBA", (S, S), (0, 0, 0, 0))
ImageDraw.Draw(hl).rounded_rectangle([16, 16, S - 16, S // 2], radius=100, fill=(255, 255, 255, 10))
img = Image.alpha_composite(img, hl)
d = ImageDraw.Draw(img)

ORANGE = (244, 100, 20, 255)

# 放大镜：镜环镂空（透出深底），白色闪电才清晰
cx, cy, r = 228, 228, 148
ring_w = 36
d.ellipse([cx - r, cy - r, cx + r, cy + r], outline=ORANGE, width=ring_w)

# 手柄：45° 向右下，圆头
ang = math.radians(45)
half = ring_w / 2
sx = cx + (r - 6) * math.cos(ang)
sy = cy + (r - 6) * math.sin(ang)
ex = cx + (r + 118) * math.cos(ang)
ey = cy + (r + 118) * math.sin(ang)
d.line([sx, sy, ex, ey], fill=ORANGE, width=62)
d.ellipse([ex - 31, ey - 31, ex + 31, ey + 31], fill=ORANGE)

# 镜内白色闪电（高速搜索）
d.polygon(
    [(258, 118), (172, 272), (238, 272), (198, 338), (292, 212), (226, 212)],
    fill=(255, 255, 255, 246),
)

img.save(here / "rust-seek-256.png")
img.resize((64, 64), Image.LANCZOS).save(here / "rust-seek-64.png")
img.save(
    here / "rust-seek.ico",
    sizes=[(256, 256), (128, 128), (64, 64), (48, 48), (32, 32), (24, 24), (16, 16)],
)
print("saved:", here)
