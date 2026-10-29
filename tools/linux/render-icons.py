#!/usr/bin/env python3
"""Render the Stampede icon as hicolor PNGs (squircle, transparent corners).

Usage: render-icons.py OUT_DIR [SIZE ...]      (default sizes 48 64 128 256 512)
Writes OUT_DIR/<N>x<N>/stampede.png, reusing the artwork in tools/icon/.
Needs Pillow + numpy (same as tools/icon/render-icon.py).
"""
import importlib.util
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ICON = os.path.normpath(os.path.join(HERE, "..", "icon"))
sys.path.insert(0, ICON)
spec = importlib.util.spec_from_file_location("render_icon", os.path.join(ICON, "render-icon.py"))
ri = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ri)


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    out = sys.argv[1]
    sizes = [int(s) for s in sys.argv[2:]] or [48, 64, 128, 256, 512]
    bg = ri.art.to_image_rgb(ri.background_rgb())
    fg = ri.art.to_image_rgba_premult(ri.foreground_layer())
    full = ri.composite(bg, fg)
    for n in sizes:
        ri.save(ri.masked(full, "squircle", n), os.path.join(out, f"{n}x{n}", "stampede.png"))
    print(f"icons {sizes} -> {out}")


if __name__ == "__main__":
    main()
