"""Draws the app icon. Usage: python3 assets/make_icon.py assets/icon.png
(scripts/bundle-mac.sh turns it into icon.icns)."""
import math, sys
from PIL import Image, ImageDraw, ImageFilter

S = 4096          # supersampled canvas, downscaled to 1024
k = S / 1024

def rr_mask(size, box, radius):
    m = Image.new("L", size, 0)
    ImageDraw.Draw(m).rounded_rectangle(box, radius=radius, fill=255)
    return m

def vgrad(w, h, top, bottom):
    g = Image.new("L", (1, h))
    for y in range(h):
        t = y / (h - 1)
        g.putpixel((0, y), round(top + (bottom - top) * t))
    return g.resize((w, h))

def shadow(alpha, blur, offset, opacity):
    sh = Image.new("RGBA", alpha.size, (0, 0, 0, 0))
    a = alpha.point(lambda v: v * opacity)
    sh.putalpha(a.filter(ImageFilter.GaussianBlur(blur)))
    out = Image.new("RGBA", alpha.size, (0, 0, 0, 0))
    out.paste(sh, offset, sh)
    return out

def gray(img_l, alpha):
    rgba = Image.merge("RGBA", (img_l, img_l, img_l, alpha))
    return rgba

icon = Image.new("RGBA", (S, S), (0, 0, 0, 0))

# Tile (macOS grid: 824pt body, 100pt margin).
m = 100 * k
tile_box = (m, m, S - m, S - m)
tile_mask = rr_mask((S, S), tile_box, 185 * k)
icon.alpha_composite(shadow(tile_mask, 18 * k, (0, int(12 * k)), 0.45))
tile = gray(vgrad(S, S, 62, 16), tile_mask)
icon.alpha_composite(tile)
# Hairline highlight along the top edge.
hl = Image.new("L", (S, S), 0)
ImageDraw.Draw(hl).rounded_rectangle(tile_box, radius=185 * k, outline=255, width=int(3 * k))
hl = Image.composite(hl, Image.new("L", (S, S), 0), vgrad(S, S, 90, 0))
icon.alpha_composite(gray(Image.new("L", (S, S), 255), hl))

def card(paper, photo_top, photo_bottom, scene):
    """A print: paper border around a grayscale landscape."""
    w, h = int(520 * k), int(400 * k)
    c = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    pm = rr_mask((w, h), (0, 0, w - 1, h - 1), 22 * k)
    c.alpha_composite(gray(Image.new("L", (w, h), paper), pm))
    b = int(26 * k)
    iw, ih = w - 2 * b, h - 2 * b
    img = gray(vgrad(iw, ih, photo_top, photo_bottom), rr_mask((iw, ih), (0, 0, iw - 1, ih - 1), 8 * k))
    if scene:
        d = ImageDraw.Draw(img)
        d.ellipse((iw * 0.66, ih * 0.14, iw * 0.66 + ih * 0.22, ih * 0.36), fill=(225, 225, 225, 255))
        d.polygon([(0, ih), (0, ih * 0.72), (iw * 0.30, ih * 0.38), (iw * 0.55, ih * 0.70), (iw * 0.55, ih)], fill=(128, 128, 128, 255))
        d.polygon([(iw * 0.30, ih), (iw * 0.62, ih * 0.50), (iw, ih * 0.82), (iw, ih)], fill=(95, 95, 95, 255))
        img.putalpha(rr_mask((iw, ih), (0, 0, iw - 1, ih - 1), 8 * k))
    c.alpha_composite(img, (b, b))
    return c

def place(layer, angle, center):
    r = layer.rotate(angle, resample=Image.BICUBIC, expand=True)
    full = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    full.alpha_composite(r, (int(center[0] - r.width / 2), int(center[1] - r.height / 2)))
    icon.alpha_composite(shadow(full.getchannel("A"), 22 * k, (0, int(14 * k)), 0.55))
    icon.alpha_composite(full)

cx, cy = S / 2, S / 2
place(card(150, 70, 45, False), 9, (cx + 34 * k, cy - 40 * k))
place(card(242, 78, 34, True), -5, (cx - 20 * k, cy + 10 * k))

# Star badge: the "pick".
bx, by, br = cx + 230 * k, cy + 205 * k, 118 * k
badge = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(badge)
d.ellipse((bx - br, by - br, bx + br, by + br), fill=(250, 250, 250, 255))
d.ellipse((bx - br + 10 * k, by - br + 10 * k, bx + br - 10 * k, by + br - 10 * k), fill=(26, 26, 26, 255))
pts = []
for i in range(10):
    a = -math.pi / 2 + i * math.pi / 5
    r = (74 if i % 2 == 0 else 30) * k
    pts.append((bx + r * math.cos(a), by + 4 * k + r * math.sin(a)))
d.polygon(pts, fill=(250, 250, 250, 255))
icon.alpha_composite(shadow(badge.getchannel("A"), 16 * k, (0, int(10 * k)), 0.6))
icon.alpha_composite(badge)

icon.resize((1024, 1024), Image.LANCZOS).save(sys.argv[1])
