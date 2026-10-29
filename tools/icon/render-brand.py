#!/usr/bin/env python3
"""Render Stampede's brand art (splash and menus) into app/assets/brand/.

  ostrich_run.png  running-ostrich silhouette, mid-stride, white on transparent, 1024 px wide
  head_profile.png the launcher-icon artwork without its background, 1024 px square
  dust_puff.png    a soft dust cloud, 512 px square
  btc_coin.png     a gold bitcoin coin, 256 px square

Usage:  python3 tools/icon/render-brand.py [--preview-dir DIR]
        --preview-dir also writes wordmark-preview.png (STAMPEDE in the display font)
        and brand-sheet.png (everything on the sunset gradient) into DIR.
Needs:  Pillow, numpy. Font: app/assets/fonts/lilita-one (OFL).
"""

import argparse
import math
import os
import sys

import numpy as np
from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import art  # noqa: E402

ROOT = os.path.normpath(os.path.join(HERE, "..", ".."))
OUT = os.path.join(ROOT, "app", "assets", "brand")
FONT = os.path.join(ROOT, "app", "assets", "fonts", "lilita-one", "LilitaOne-Regular.ttf")


# ── pixel-space helpers (local to this script) ────────────────────────────────

def bez(p0, p1, p2, p3, n=120):
    t = np.linspace(0, 1, n)[:, None]
    a, b, c, d = (1 - t) ** 3, 3 * (1 - t) ** 2 * t, 3 * (1 - t) * t * t, t ** 3
    return a * np.array(p0, float) + b * np.array(p1, float) + c * np.array(p2, float) + d * np.array(p3, float)


class Canvas:
    """A supersampled alpha mask in pixel coordinates (w x h at the output size)."""

    def __init__(self, w, h, ss=3):
        self.w, self.h, self.ss = w, h, ss
        self.img = Image.new("L", (w * ss, h * ss), 0)
        self.d = ImageDraw.Draw(self.img)

    def poly(self, pts, fill=255):
        self.d.polygon([(x * self.ss, y * self.ss) for x, y in pts], fill=fill)

    def ellipse(self, c, rx, ry, angle=0.0, fill=255):
        t = np.linspace(0, 2 * math.pi, 180, endpoint=False)
        a = math.radians(angle)
        x, y = rx * np.cos(t), ry * np.sin(t)
        self.poly(np.stack([c[0] + x * math.cos(a) - y * math.sin(a), c[1] + x * math.sin(a) + y * math.cos(a)], 1), fill)

    def stroke(self, pts, w0, w1=None, fill=255, ease=1.0):
        """Variable-width stroke along a polyline (discs stamped along it)."""
        w1 = w0 if w1 is None else w1
        t = np.linspace(0, 1, len(pts)) ** ease
        for (x, y), w in zip(pts, w0 + (w1 - w0) * t):
            r = w * self.ss / 2
            self.d.ellipse((x * self.ss - r, y * self.ss - r, x * self.ss + r, y * self.ss + r), fill=fill)

    def array(self):
        im = self.img.resize((self.w, self.h), Image.LANCZOS)
        return np.asarray(im, dtype=np.float32) / 255.0


def white_rgba(alpha):
    out = np.dstack([np.ones_like(alpha)] * 3 + [np.clip(alpha, 0, 1)])
    return Image.fromarray((out * 255 + 0.5).astype(np.uint8), "RGBA")


# ── running ostrich ───────────────────────────────────────────────────────────

def ostrich_run(w=1024, h=700):
    """Mid-stride, facing right: chest up, neck stretched, one leg reaching forward and
    the other driving off behind, wing flared, tail plumes streaming back."""
    s = w / 1024.0

    def P(x, y):
        return (x * s, y * s)

    body = Canvas(w, h)
    # Torso: a deep oval plus scalloped plumage round the rear and underside.
    body.ellipse(P(455, 290), 205 * s, 114 * s, 5)
    for i in range(10):
        a = math.radians(150 + i * 15)
        cx, cy = 455 + 205 * math.cos(a), 290 + 114 * math.sin(a) + 4
        body.ellipse(P(cx, cy), 40 * s, 32 * s, i * 20)
    for i in range(6):
        body.ellipse(P(340 + i * 52, 395 - 6 * math.sin(i)), 36 * s, 24 * s, 8)
    # Neck: an S-curve up and forward, thick at the shoulders.
    neck = bez(P(575, 285), P(670, 250), P(610, 120), P(760, 98), n=220)
    body.stroke(neck, 78 * s, 28 * s, ease=0.75)
    # Head and a short, flat beak.
    body.ellipse(P(785, 92), 50 * s, 30 * s, 8)
    body.poly([P(800, 76), P(850, 80), P(895, 100), P(896, 114), P(852, 124), P(805, 112)])
    body.ellipse(P(893, 107), 10 * s, 8 * s, 20)
    # Tail plumes streaming back and up.
    for k, (a0, ln, wd) in enumerate([(-166, 235, 70), (-176, 265, 74), (-188, 240, 66), (-199, 200, 56), (-150, 190, 58)]):
        a = math.radians(a0)
        p0 = (285 + k * 8, 255 + k * 14)
        d = np.array([math.cos(a), math.sin(a)])
        p3 = np.array(p0) + d * ln
        pts = bez(P(*p0), P(p0[0] + d[0] * ln * 0.4, p0[1] + d[1] * ln * 0.2 - 22), P(p3[0] - 30, p3[1] - 40), P(*p3))
        body.stroke(pts, wd * s, 8 * s, ease=0.7)

    def leg(hip, hock, foot, toe, t0, t1, t2):
        up = bez(P(*hip), P(*hip), P(*hock), P(*hock), 70)
        low = bez(P(*hock), P(*hock), P(*foot), P(*foot), 70)
        body.stroke(up, t0 * s, t1 * s)
        body.stroke(low, t1 * s, t2 * s)
        body.ellipse(P(*hock), t1 * s * 0.62, t1 * s * 0.62)
        # Foot: a broad main toe and a small one.
        body.stroke(bez(P(*foot), P(*foot), P(*toe), P(*toe), 30), t2 * s * 1.1, 20 * s)
        small = (toe[0] - (toe[0] - foot[0]) * 0.1, toe[1] + 20)
        body.stroke(bez(P(*foot), P(*foot), P(*small), P(*small), 30), t2 * s, 14 * s)
        body.ellipse(P(*toe), 16 * s, 12 * s, 25)

    leg((520, 368), (575, 478), (665, 520), (748, 546), 100, 52, 30)    # front leg reaching
    leg((395, 378), (365, 474), (262, 548), (190, 566), 96, 50, 28)      # back leg driving
    b = body.array()

    # Wing: a fan of long feathers flared up and back, over the torso.
    wing = Canvas(w, h)
    root = (480, 250)
    for k, ang in enumerate([-178, -166, -154, -142, -130, -118, -106]):
        a = math.radians(ang)
        ln = 250 - k * 14
        tip = (root[0] + ln * math.cos(a), root[1] + ln * math.sin(a))
        mid = (root[0] + ln * 0.5 * math.cos(a + 0.2), root[1] + ln * 0.5 * math.sin(a + 0.2))
        wing.stroke(bez(P(*root), P(*mid), P(*mid), P(*tip), 80), 62 * s, 10 * s, ease=0.8)
    wing.ellipse(P(485, 262), 86 * s, 52 * s, -18)
    out = np.maximum(b, wing.array())

    # Negative-space details: the eye, and a curved line where the wing meets the torso.
    cut = Canvas(w, h)
    cut.ellipse(P(797, 87), 10 * s, 10 * s)
    cut.stroke(bez(P(420, 312), P(500, 340), P(580, 322), P(612, 270), 80), 7 * s, 3 * s)
    out = out * (1 - cut.array())
    return white_rgba(out)


# ── dust puff ──────────────────────────────────────────────────────────────────

def noise_field(size, rng, octaves=((8, 1.0), (16, 0.5), (32, 0.25))):
    acc = np.zeros((size, size), dtype=np.float32)
    for cells, amp in octaves:
        small = rng.random((cells, cells)).astype(np.float32)
        big = Image.fromarray((small * 255).astype(np.uint8), "L").resize((size, size), Image.BICUBIC)
        acc += amp * (np.asarray(big, dtype=np.float32) / 255.0 - 0.5)
    return acc


def dust_puff(size=512):
    """A billowing dust cloud: overlapping spherical lobes lit from the top right,
    creased where they meet, with soft, fluffy translucent edges."""
    rng = np.random.default_rng(5)
    k = size / 512.0
    yy, xx = np.mgrid[0:size, 0:size].astype(np.float32)
    lobes = [(256, 300, 112), (160, 312, 78), (356, 314, 82), (200, 232, 74), (314, 228, 72), (258, 184, 64), (104, 356, 46), (410, 358, 48), (256, 352, 96)]
    height = np.zeros((size, size), dtype=np.float32)
    for cx, cy, r in lobes:
        d2 = ((xx - cx * k) ** 2 + (yy - cy * k) ** 2) / (r * k) ** 2
        height = np.maximum(height, np.sqrt(np.clip(1 - d2, 0, 1)) * r * k)
    height /= 120 * k
    wob = noise_field(size, rng, octaves=((10, 1.0), (24, 0.5), (48, 0.3)))
    h = art.blur(height, 3.0 * k) + 0.07 * wob * (height > 0.02)
    gy, gx = np.gradient(art.blur(h, 2.5 * k))
    nx, ny = -gx * 60 * k, -gy * 60 * k
    inv = 1 / np.sqrt(nx * nx + ny * ny + 1)
    lit = np.clip((nx * 0.5 + ny * -0.62 + 0.62) * inv / 1.0, 0, 1)
    # Fluffy coverage: soft threshold on the (noisy) height.
    cover = art.smoothstep(0.03, 0.30, h)
    alpha = cover * 0.95
    alpha *= 1 - art.smoothstep(0.84, 0.97, yy / size)
    col = art.ramp(lit, [(0.0, art.lerp(art.PLUM, art.SAND, 0.4)), (0.45, art.SAND), (0.85, art.lerp(art.SAND, art.OFFWHITE, 0.6)), (1.0, art.OFFWHITE)])
    out = np.dstack([col, np.clip(alpha, 0, 1)])
    return Image.fromarray((out * 255 + 0.5).astype(np.uint8), "RGBA")


# ── bitcoin coin ────────────────────────────────────────────────────────────────

def btc_coin(size=256):
    S = 1024
    yy, xx = np.mgrid[0:S, 0:S].astype(np.float32)
    cx = cy = S / 2
    r = np.hypot(xx - cx, yy - cy) / (S / 2 * 0.9)
    edge = np.clip((0.9 * S / 2 - np.hypot(xx - cx, yy - cy)) * 0.5 + 0.5, 0, 1)
    # Face: gold radial/linear blend, a darker rim, a bevel line.
    t = np.clip((yy - cy) / (S * 0.45) * 0.5 + 0.5 - (xx - cx) / (S * 0.45) * 0.18, 0, 1)
    face = art.ramp(t, [(0.0, art.BTC_HI), (0.5, art.BTC), (1.0, hexrgb("#B9560B"))])
    rim = art.smoothstep(0.80, 0.84, r) * (1 - art.smoothstep(0.96, 1.0, r))
    face = face + rim[..., None] * (art.ramp(t, [(0, art.OFFWHITE * 0.9), (0.5, art.BTC_HI), (1, art.BTC)]) - face) * 0.9
    groove = np.exp(-((r - 0.78) / 0.012) ** 2)
    face = face * (1 - 0.35 * groove[..., None])
    inner = art.smoothstep(0.78, 0.74, r)
    face = face + inner[..., None] * (art.ramp(t, [(0, art.lerp(art.BTC_HI, art.BTC, 0.2)), (0.6, art.BTC), (1, art.BTC_LO)]) - face) * 0.6
    # The B with its two strokes: Lilita One's B plus bars, embossed.
    font = ImageFont.truetype(FONT, int(S * 0.62))
    glyph = Image.new("L", (S * 2, S * 2), 0)
    gd = ImageDraw.Draw(glyph)
    gd.text((S, S), "B", font=font, fill=255, anchor="mm")
    bb = glyph.getbbox()
    gcx = (bb[0] + bb[2]) / 2
    gd.rectangle((gcx - S * 0.095, bb[1] - S * 0.09, gcx - S * 0.095 + S * 0.05, bb[1] + S * 0.04), fill=255)
    gd.rectangle((gcx + S * 0.045, bb[1] - S * 0.09, gcx + S * 0.045 + S * 0.05, bb[1] + S * 0.04), fill=255)
    gd.rectangle((gcx - S * 0.095, bb[3] - S * 0.04, gcx - S * 0.095 + S * 0.05, bb[3] + S * 0.09), fill=255)
    gd.rectangle((gcx + S * 0.045, bb[3] - S * 0.04, gcx + S * 0.045 + S * 0.05, bb[3] + S * 0.09), fill=255)
    bb = glyph.getbbox()
    glyph = glyph.crop(bb)
    gw, gh = glyph.size
    canvas = Image.new("L", (S, S), 0)
    canvas.paste(glyph, ((S - gw) // 2, (S - gh) // 2 + 6))
    g = np.asarray(canvas, dtype=np.float32) / 255.0
    g = art.blur(g, 0.8)
    # Glyph in deep burnt orange.
    gcol = art.ramp(t, [(0, hexrgb("#C2540E")), (1, hexrgb("#8E3A08"))])
    face = face * (1 - g[..., None]) + gcol * g[..., None]
    # Soft light on the top-left, and a sparkle.
    glow = np.exp(-(((xx - cx * 0.62) ** 2 + (yy - cy * 0.55) ** 2) / (S * 0.28) ** 2))
    face = face + art.OFFWHITE * glow[..., None] * 0.18 * (1 - g[..., None])
    sp = np.exp(-(((xx - S * 0.30) ** 2 + (yy - S * 0.27) ** 2) / (S * 0.035) ** 2))
    face += sp[..., None] * 0.7
    face = np.clip(face, 0, 1)
    # Drop shadow under the coin.
    sh = art.blur(art.shift(edge, int(S * 0.025), 0), S * 0.018) * 0.45
    a = np.clip(edge + sh * (1 - edge), 0, 1)
    rgb = np.where(a[..., None] > 1e-4, (face * edge[..., None]) / np.maximum(a[..., None], 1e-4), 0)
    rgb = np.clip(rgb + (1 - edge[..., None]) * 0, 0, 1)
    out = np.dstack([rgb, a])
    im = Image.fromarray((out * 255 + 0.5).astype(np.uint8), "RGBA")
    return im.resize((size, size), Image.LANCZOS)


def drop_bit(img, bits=7):
    """Reduce every channel to `bits` bits (keeping full-range white/black) so the
    PNG deflates better; invisible on smooth glows at 7 bits."""
    a = np.asarray(img)
    q = (a >> (8 - bits)) << (8 - bits)
    q = q | (q >> bits)
    return Image.fromarray(q.astype(np.uint8), img.mode)


def hexrgb(s):
    return art.hexc(s)


# ── wordmark preview (and the layout the splash should use) ───────────────────

def wordmark(text="STAMPEDE", height=220):
    """The wordmark in Lilita One: ember-to-gold gradient face, plum outline, drop shadow."""
    font = ImageFont.truetype(FONT, height)
    pad = int(height * 0.35)
    probe = Image.new("L", (10, 10))
    l, t, r, b = ImageDraw.Draw(probe).textbbox((0, 0), text, font=font)
    w, h = r - l + pad * 2, b - t + pad * 2
    m = Image.new("L", (w, h), 0)
    ImageDraw.Draw(m).text((pad - l, pad - t), text, font=font, fill=255)
    face = np.asarray(m, dtype=np.float32) / 255.0
    from PIL import ImageFilter
    stroke = Image.new("L", (w, h), 0)
    ImageDraw.Draw(stroke).text((pad - l, pad - t), text, font=font, fill=255, stroke_width=int(height * 0.07), stroke_fill=255)
    outline = np.asarray(stroke, dtype=np.float32) / 255.0
    yy = np.mgrid[0:h, 0:w][0].astype(np.float32)
    ty = np.clip((yy - pad) / max(b - t, 1), 0, 1)
    col = art.ramp(ty, [(0.0, art.BTC_HI), (0.45, art.BTC), (0.8, art.BTC_LO), (1.0, hexrgb("#C23A14"))])
    img = np.zeros((h, w, 4), dtype=np.float32)
    # Shadow, outline, face, highlight.
    shadow = art.blur(art.shift(outline, int(height * 0.06), int(height * 0.02)), height * 0.04)
    img[..., :3] = art.INK * shadow[..., None]
    img[..., 3] = shadow * 0.6
    ink_col = art.lerp(art.INK, art.PLUM, 0.35)
    a = outline
    img[..., :3] = img[..., :3] * (1 - a[..., None]) + ink_col * a[..., None]
    img[..., 3] = img[..., 3] * (1 - a) + a
    img[..., :3] = img[..., :3] * (1 - face[..., None]) + col * face[..., None]
    img[..., 3] = np.maximum(img[..., 3], face)
    top = np.clip(face - art.shift(face, int(height * 0.045), 0), 0, 1) * (ty < 0.5)
    img[..., :3] += art.OFFWHITE * top[..., None] * 0.6
    img = np.clip(img, 0, 1)
    rgb = np.where(img[..., 3:4] > 1e-4, img[..., :3], 0)
    return Image.fromarray((np.dstack([rgb, img[..., 3]]) * 255 + 0.5).astype(np.uint8), "RGBA")


def contact_sheet(path, run, head, puff, coin, word):
    W, H = 1500, 900
    sky = art.ramp(np.linspace(0, 1, H)[:, None] * np.ones((1, W)), [(0, art.INDIGO), (0.45, art.PLUM), (0.8, art.lerp(art.PLUM, art.EMBER, 0.7)), (1, art.EMBER)])
    sheet = Image.fromarray((sky * 255).astype(np.uint8), "RGB").convert("RGBA")
    ww = word.resize((int(word.width * 0.75), int(word.height * 0.75)), Image.LANCZOS)
    sheet.alpha_composite(ww, ((W - ww.width) // 2, 20))
    sheet.alpha_composite(run.resize((700, int(700 * run.height / run.width)), Image.LANCZOS), (30, 330))
    sheet.alpha_composite(head.resize((420, 420), Image.LANCZOS), (760, 300))
    sheet.alpha_composite(puff.resize((256, 256), Image.LANCZOS), (1200, 330))
    sheet.alpha_composite(coin, (1120, 620))
    sheet.alpha_composite(coin.resize((96, 96), Image.LANCZOS), (1400, 640))
    sheet.alpha_composite(coin.resize((48, 48), Image.LANCZOS), (1400, 760))
    sheet.save(path)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--preview-dir", help="also write wordmark-preview.png and brand-sheet.png here")
    args = ap.parse_args()
    os.makedirs(OUT, exist_ok=True)

    run = ostrich_run()
    run.save(os.path.join(OUT, "ostrich_run.png"), optimize=True)
    print("ostrich_run.png", run.size)

    fg = art.render_ostrich()
    head = art.to_image_rgba_premult(fg).resize((1024, 1024), Image.LANCZOS)
    head = drop_bit(head)   # 7-bit channels: smooth glows deflate far better (485 KB -> ~390 KB)
    head.save(os.path.join(OUT, "head_profile.png"), optimize=True)
    print("head_profile.png", head.size)

    puff = dust_puff()
    puff.save(os.path.join(OUT, "dust_puff.png"), optimize=True)
    print("dust_puff.png", puff.size)

    coin = btc_coin()
    coin.save(os.path.join(OUT, "btc_coin.png"), optimize=True)
    print("btc_coin.png", coin.size)

    total = sum(os.path.getsize(os.path.join(OUT, f)) for f in os.listdir(OUT))
    print(f"total {total / 1024:.0f} KB in {OUT}")

    if args.preview_dir:
        os.makedirs(args.preview_dir, exist_ok=True)
        word = wordmark()
        word.save(os.path.join(args.preview_dir, "wordmark-preview.png"))
        contact_sheet(os.path.join(args.preview_dir, "brand-sheet.png"), run, head, puff, coin, word)
        print("previews in", args.preview_dir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
