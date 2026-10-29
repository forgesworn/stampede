#!/usr/bin/env python3
"""Render the Stampede launcher icon (adaptive + legacy) into app/android/res.

The picture: a cocky ostrich head and neck in profile, laser beams blazing from
its eye, a bitcoin-orange sunset disc behind, a dust swoosh, on a deep
indigo -> plum -> ember desert-sunset gradient (SPEC section 13). Drawing code is
in art.py (shared with render-brand.py).

Outputs, per density mdpi..xxxhdpi: ic_launcher_foreground / _background /
_monochrome (108 dp layers), ic_launcher and ic_launcher_round (48 dp legacy),
plus mipmap-anydpi-v26/ic_launcher.xml and ic_launcher_round.xml.

Usage:  python3 tools/icon/render-icon.py [--preview OUT.png]
Needs:  Pillow, numpy.
"""

import argparse
import os
import sys

import numpy as np
from PIL import Image, ImageDraw

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import art  # noqa: E402
from art import *  # noqa: E402,F401,F403

HERE = os.path.dirname(os.path.abspath(__file__))
RES = os.path.normpath(os.path.join(HERE, "..", "..", "app", "android", "res"))

DENSITIES = {"mdpi": 1.0, "hdpi": 1.5, "xhdpi": 2.0, "xxhdpi": 3.0, "xxxhdpi": 4.0}

SUN_C = (38.0, 69.0)
SUN_R = 28.0


def background_rgb():
    """Desert sunset: indigo sky to plum to ember, a retro-banded bitcoin sun, dunes."""
    xx, yy = grid()
    t = yy / 108.0
    sky = ramp(t, [(0.0, INDIGO), (0.30, INDIGO2), (0.52, PLUM), (0.74, lerp(PLUM, EMBER, 0.65)), (1.0, EMBER)])
    img = sky.astype(np.float32)

    # Stars, a few, in the upper sky only.
    rng = np.random.default_rng(3)
    stars = np.zeros((MASTER, MASTER), dtype=np.float32)
    for _ in range(26):
        x, y = rng.uniform(4, 104), rng.uniform(2, 34)
        if np.hypot(x - SUN_C[0], y - SUN_C[1]) < SUN_R + 4:
            continue
        r = px(rng.uniform(0.18, 0.42))
        cx, cy = x * K, y * K
        x0, x1, y0, y1 = int(cx - 4 * r), int(cx + 4 * r) + 1, int(cy - 4 * r), int(cy + 4 * r) + 1
        sx, sy = np.meshgrid(np.arange(x0, x1), np.arange(y0, y1))
        stars[y0:y1, x0:x1] += rng.uniform(0.35, 0.9) * np.exp(-((sx - cx) ** 2 + (sy - cy) ** 2) / (r * r))
    img += stars[..., None] * OFFWHITE[None, None, :] * (1 - smoothstep(0, 40, yy))[..., None]

    # The sun: halo, then a disc graded gold to burnt orange with retro slats.
    r = np.hypot(xx - SUN_C[0], yy - SUN_C[1])
    halo = np.exp(-((r - SUN_R * 0.2) / (SUN_R * 1.35)) ** 2)
    img += (BTC * 0.55)[None, None, :] * halo[..., None] * 0.55
    disc = np.clip((SUN_R - r) * K * 0.5 + 0.5, 0, 1)   # 1 px antialiased edge
    gy = np.clip((yy - (SUN_C[1] - SUN_R)) / (2 * SUN_R), 0, 1)
    scol = ramp(gy, [(0.0, BTC_HI), (0.38, BTC), (0.78, BTC_LO), (1.0, hexc("#C23A14"))])
    # Slats: dark bands that widen towards the bottom of the disc.
    slat = np.zeros_like(yy)
    for i in range(5):
        y0 = SUN_C[1] + 3.0 + i * 4.6
        h = 0.55 + i * 0.5
        slat = np.maximum(slat, smoothstep(0, 0.35, h / 2 - np.abs(yy - y0)) * (yy > SUN_C[1] - 1))
    sky_through = sky * 0.9
    scol = scol * (1 - slat[..., None]) + sky_through * slat[..., None]
    img = img * (1 - disc[..., None]) + scol * disc[..., None]
    # Soft inner glow on the upper rim.
    rim = np.exp(-(((SUN_R - r) - 0.0) / 1.4) ** 2) * disc * smoothstep(SUN_C[1] + 6, SUN_C[1] - 20, yy)
    img += (BTC_HI * 0.45)[None, None, :] * rim[..., None] * 0.5

    # Dunes: two layered silhouettes with a thin rim light on the crest.
    for base, amp, ph, fq, col, rimc in [
        (85.0, 2.6, 0.9, 0.075, lerp(PLUM, INK, 0.5), BTC),
        (94.0, 2.2, 3.1, 0.095, INK, lerp(EMBER, PLUM, 0.2)),
    ]:
        crest = base + amp * np.sin(xx * 0.05 + ph) + 1.1 * np.sin(xx * 0.13 + ph * 2)
        m = np.clip((yy - crest) * K * 0.5 + 0.5, 0, 1)
        edge = np.exp(-((yy - crest) / 0.55) ** 2) * (yy >= crest - 0.2)
        img = img * (1 - m[..., None]) + col[None, None, :] * m[..., None]
        img += rimc[None, None, :] * edge[..., None] * 0.35

    # Vignette to pull the eye to the centre.
    v = 1.0 - 0.28 * np.clip(((xx - 54) ** 2 + (yy - 54) ** 2) / (62 ** 2), 0, 1)
    img *= v[..., None]
    return np.clip(img, 0, 1)


def foreground_layer():
    return art.render_ostrich()


def monochrome_image():
    """Themed-icon layer (API 33): the figure and beams as white alpha."""
    sil = art.figure_mask(include_beams=True)
    a = np.clip(sil * 1.0, 0, 1)
    out = np.dstack([np.ones_like(a), np.ones_like(a), np.ones_like(a), a])
    return Image.fromarray((out * 255 + 0.5).astype(np.uint8), "RGBA")


def composite(bg_img, fg_img):
    out = bg_img.convert("RGBA")
    out.alpha_composite(fg_img)
    return out


def masked(img, shape, size):
    """Legacy icon: the central 72 of 108 dp, masked to a squircle or circle."""
    u = lambda v: int(v * K)
    crop = img.crop((u(18), u(18), u(90), u(90)))
    cs = crop.size[0]
    big = Image.new("L", (cs * 4, cs * 4), 0)
    d = ImageDraw.Draw(big)
    pad = int(cs * 4 * 0.015)
    if shape == "circle":
        d.ellipse((pad, pad, cs * 4 - pad, cs * 4 - pad), fill=255)
    else:
        d.rounded_rectangle((pad, pad, cs * 4 - pad, cs * 4 - pad), radius=int(cs * 4 * 0.22), fill=255)
    alpha = big.resize((cs, cs), Image.LANCZOS)
    crop = crop.copy()
    crop.putalpha(alpha)
    return crop.resize((size, size), Image.LANCZOS)


def save(img, path):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    img.save(path, optimize=True)


XML = """<?xml version="1.0" encoding="utf-8"?>
<!-- Generated layers: tools/icon/render-icon.py -->
<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">
    <background android:drawable="@mipmap/ic_launcher_background" />
    <foreground android:drawable="@mipmap/ic_launcher_foreground" />
    <monochrome android:drawable="@mipmap/ic_launcher_monochrome" />
</adaptive-icon>
"""


def write_preview(path, full, mono, bg, fg):
    pad = 28
    sheet = Image.new("RGBA", (1500, 860), (30, 28, 44, 255))
    d = ImageDraw.Draw(sheet)
    # Big circle + squircle masks.
    sheet.alpha_composite(masked(full, "circle", 512), (pad, pad))
    sheet.alpha_composite(masked(full, "squircle", 320), (pad + 512 + pad, pad))
    # Small sizes, each on its own chip: 48, 96, 192 under circle and squircle masks.
    x = pad + 512 + pad + 320 + pad
    y = pad
    for size in (192, 96, 48):
        sheet.alpha_composite(masked(full, "circle", size), (x, y))
        sheet.alpha_composite(masked(full, "squircle", size), (x + size + 16, y))
        y += size + 18
    # The raw full-bleed 512 px, the themed monochrome, and the foreground alone.
    y2 = pad + 512 + pad
    sheet.alpha_composite(full.resize((256, 256), Image.LANCZOS), (pad, y2))
    sheet.alpha_composite(masked(composite(Image.new("RGB", full.size, (32, 58, 52)), mono), "circle", 256), (pad + 256 + pad, y2))
    chk = Image.new("RGBA", fg.size, (60, 60, 70, 255))
    sheet.alpha_composite(masked(composite(chk.convert("RGB"), fg), "squircle", 256), (pad + 2 * (256 + pad), y2))
    d.text((pad + 3 * (256 + pad), y2 + 8), "512 / 320 / 192 / 96 / 48 px\ncircle + squircle masks\nbottom: full-bleed, themed\nmono, foreground alone", fill=(210, 205, 230, 255))
    save(sheet, path)
    print(f"wrote preview {path}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--preview", help="write a preview sheet PNG here")
    ap.add_argument("--no-write", action="store_true", help="do not write the res/ files (preview only)")
    args = ap.parse_args()

    bg_arr = background_rgb()
    fg_p = foreground_layer()
    bg = art.to_image_rgb(bg_arr)
    fg = art.to_image_rgba_premult(fg_p)
    mono = monochrome_image()
    full = composite(bg, fg)

    if not args.no_write:
        for name, k in DENSITIES.items():
            d = os.path.join(RES, f"mipmap-{name}")
            layer_px = int(round(108 * k))
            legacy = int(round(48 * k))
            save(fg.resize((layer_px, layer_px), Image.LANCZOS), os.path.join(d, "ic_launcher_foreground.png"))
            save(bg.resize((layer_px, layer_px), Image.LANCZOS), os.path.join(d, "ic_launcher_background.png"))
            save(mono.resize((layer_px, layer_px), Image.LANCZOS), os.path.join(d, "ic_launcher_monochrome.png"))
            save(masked(full, "squircle", legacy), os.path.join(d, "ic_launcher.png"))
            save(masked(full, "circle", legacy), os.path.join(d, "ic_launcher_round.png"))
        any26 = os.path.join(RES, "mipmap-anydpi-v26")
        os.makedirs(any26, exist_ok=True)
        for f in ("ic_launcher.xml", "ic_launcher_round.xml"):
            with open(os.path.join(any26, f), "w") as fh:
                fh.write(XML)
        print(f"wrote launcher icons under {RES}")

    if args.preview:
        os.makedirs(os.path.dirname(os.path.abspath(args.preview)), exist_ok=True)
        write_preview(args.preview, full, mono, bg, fg)
    return 0


if __name__ == "__main__":
    sys.exit(main())
