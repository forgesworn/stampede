"""Shared drawing toolkit and the ostrich artwork for Stampede's icon and brand art.

Pillow + numpy only. Everything is drawn in "dp" units on a 108 dp square (the
Android adaptive-icon canvas, y down) and rasterised at MASTER px with extra
supersampling for hard shapes. Soft things (glows, shading) are float blurs.

Used by render-icon.py (launcher icon layers) and render-brand.py (splash/menu art).
"""

import math

import numpy as np
from PIL import Image, ImageDraw

MASTER = 1296   # 108 dp at 12 px/dp
SS = 2          # supersampling for hard shapes
K = MASTER / 108.0


def hexc(s):
    s = s.lstrip("#")
    return np.array([int(s[i:i + 2], 16) for i in (0, 2, 4)], dtype=np.float32) / 255.0


# Palette (SPEC section 13).
INDIGO = hexc("#150F3D")
INDIGO2 = hexc("#2A1A66")
PLUM = hexc("#6A1F73")
EMBER = hexc("#C2410C")
BTC = hexc("#F7931A")
BTC_HI = hexc("#FFD36B")
BTC_LO = hexc("#E2531A")
SAND = hexc("#E8C68A")
OFFWHITE = hexc("#FFF4E4")
SKIN = hexc("#F4D9BE")
SKIN_SHADE = hexc("#A8587C")
INK = hexc("#1D0B2C")
LASER = hexc("#FF2438")
LASER_HOT = hexc("#FFB3A8")


# ── numpy helpers ────────────────────────────────────────────────────────────

def blur(a, sigma_px):
    """Gaussian-ish blur (three box passes) of a 2D or 3D float array."""
    if sigma_px < 0.3:
        return a
    n = 3
    w_ideal = math.sqrt(12 * sigma_px * sigma_px / n + 1)
    wl = int(math.floor(w_ideal))
    if wl % 2 == 0:
        wl -= 1
    wu = wl + 2
    m = round((12 * sigma_px * sigma_px - n * wl * wl - 4 * n * wl - 3 * n) / (-4 * wl - 4))
    out = a.astype(np.float32)
    for i in range(n):
        w = wl if i < m else wu
        out = _box(out, w, 0)
        out = _box(out, w, 1)
    return out


def _box(a, w, axis):
    r = w // 2
    pad = [(0, 0)] * a.ndim
    pad[axis] = (r + 1, r)
    p = np.pad(a, pad, mode="edge")
    c = np.cumsum(p, axis=axis, dtype=np.float64)
    hi = np.take(c, range(w, w + a.shape[axis]), axis=axis)
    lo = np.take(c, range(0, a.shape[axis]), axis=axis)
    return ((hi - lo) / w).astype(np.float32)


def shift(a, dy, dx):
    """Translate a 2D array by whole pixels, filling with zeros (a roll would wrap)."""
    dy, dx = int(dy), int(dx)
    out = np.zeros_like(a)
    h, w = a.shape[:2]
    ys, yd = (slice(0, h - dy), slice(dy, h)) if dy >= 0 else (slice(-dy, h), slice(0, h + dy))
    xs, xd = (slice(0, w - dx), slice(dx, w)) if dx >= 0 else (slice(-dx, w), slice(0, w + dx))
    out[yd, xd] = a[ys, xs]
    return out


def smoothstep(e0, e1, x):
    t = np.clip((x - e0) / (e1 - e0), 0, 1)
    return t * t * (3 - 2 * t)


# The view maps "design" coordinates (what the drawing code is written in) to canvas dp:
# canvas = A + S * design. Identity unless inside `use_view`.
_VIEW = [1.0, 0.0, 0.0]


class use_view:
    """Context manager: draw in design coordinates scaled by `s` so that design point
    `design_pt` lands on canvas point `canvas_pt` (dp)."""

    def __init__(self, s, design_pt, canvas_pt):
        self.v = [s, canvas_pt[0] - s * design_pt[0], canvas_pt[1] - s * design_pt[1]]

    def __enter__(self):
        self.old = list(_VIEW)
        _VIEW[:] = self.v

    def __exit__(self, *a):
        _VIEW[:] = self.old


def px(v):
    """A length in design dp to master px (honours the current view scale)."""
    return v * K * _VIEW[0]


def grid():
    """Per-pixel (x, y) in the *design* coordinates of the current view."""
    yy, xx = np.mgrid[0:MASTER, 0:MASTER].astype(np.float32)
    s, ax, ay = _VIEW
    return (xx / K - ax) / s, (yy / K - ay) / s


def lerp(a, b, t):
    return a + (b - a) * t


def ramp(t, stops):
    """Colour ramp: stops = [(pos, rgb), ...]; t is an array. Returns (..., 3)."""
    t = np.clip(t, 0, 1)
    out = np.zeros(t.shape + (3,), dtype=np.float32)
    for (p0, c0), (p1, c1) in zip(stops[:-1], stops[1:]):
        m = (t >= p0) & (t <= p1)
        u = ((t - p0) / max(p1 - p0, 1e-6))[..., None]
        out = np.where(m[..., None], c0 + (c1 - c0) * u, out)
    return out


# ── masks (hard shapes, supersampled) ────────────────────────────────────────

def _mask(draw_fn):
    img = Image.new("L", (MASTER * SS, MASTER * SS), 0)
    draw_fn(ImageDraw.Draw(img), K * SS)
    img = img.resize((MASTER, MASTER), Image.LANCZOS)
    return np.asarray(img, dtype=np.float32) / 255.0


def bez(p0, p1, p2, p3, n=200):
    t = np.linspace(0, 1, n)[:, None]
    a, b, c, d = (1 - t) ** 3, 3 * (1 - t) ** 2 * t, 3 * (1 - t) * t * t, t ** 3
    return a * np.array(p0) + b * np.array(p1) + c * np.array(p2) + d * np.array(p3)


def bez_chain(*segments, n=120):
    """Join several (p0,p1,p2,p3) cubic segments into one polyline."""
    pts = [bez(*s, n=n) for s in segments]
    return np.concatenate(pts, axis=0)


def m_poly(pts):
    pts = [tuple(p) for p in pts]
    vs, ax, ay = _VIEW

    def fn(d, s):
        d.polygon([((ax + vs * x) * s, (ay + vs * y) * s) for x, y in pts], fill=255)

    return _mask(fn)


def m_discs(pts, radii):
    """Union of discs along a path (a smooth variable-width stroke)."""
    radii = np.broadcast_to(np.asarray(radii, dtype=np.float32), (len(pts),))
    vs, ax, ay = _VIEW

    def fn(d, s):
        for (x, y), r in zip(pts, radii):
            cx, cy, rr = (ax + vs * x) * s, (ay + vs * y) * s, vs * r * s
            d.ellipse((cx - rr, cy - rr, cx + rr, cy + rr), fill=255)

    return _mask(fn)


def m_ellipse(c, rx, ry, angle_deg=0.0):
    a = math.radians(angle_deg)
    t = np.linspace(0, 2 * math.pi, 240, endpoint=False)
    x = rx * np.cos(t)
    y = ry * np.sin(t)
    pts = np.stack([c[0] + x * math.cos(a) - y * math.sin(a), c[1] + x * math.sin(a) + y * math.cos(a)], axis=1)
    return m_poly(pts)


def m_circle(c, r):
    return m_ellipse(c, r, r)


def tapered_stroke(pts, w0, w1, ease=1.0):
    """Disc stroke whose radius goes from w0/2 to w1/2 along the path."""
    t = np.linspace(0, 1, len(pts)) ** ease
    return m_discs(pts, (w0 + (w1 - w0) * t) / 2)


def leaf_stroke(pts, width):
    """A leaf-shaped stroke: thin at both ends, widest about two-thirds along."""
    t = np.linspace(0, 1, len(pts))
    prof = np.sin(math.pi * t ** 0.75) ** 0.9
    return m_discs(pts, np.maximum(prof * width / 2, 0.02))


def soften(m, r_dp, lo=0.35, hi=0.65):
    """Round off concave joins: blur then re-threshold (a metaball-style union)."""
    b = blur(m, px(r_dp))
    return smoothstep(lo, hi, b)


def dilate(m, r_dp):
    return smoothstep(0.08, 0.3, blur(m, px(r_dp) * 0.9))


# ── compositing (premultiplied RGBA floats) ──────────────────────────────────

def layer():
    return np.zeros((MASTER, MASTER, 4), dtype=np.float32)


def over(dst, rgb, alpha):
    """Paint colour `rgb` (3, or HxWx3) through `alpha` (HxW) over `dst` (premultiplied)."""
    alpha = np.clip(alpha, 0, 1)
    col = np.asarray(rgb, dtype=np.float32)
    if col.ndim == 1:
        col = col[None, None, :]
    dst *= (1 - alpha)[..., None]
    dst[..., :3] += col * alpha[..., None]
    dst[..., 3] += alpha


def add_light(dst, rgb, intensity):
    """Additive light: brightens colour; alpha follows brightness so it survives on transparency."""
    col = np.asarray(rgb, dtype=np.float32)
    if col.ndim == 1:
        col = col[None, None, :]
    add = col * intensity[..., None]
    dst[..., :3] += add
    dst[..., 3] = np.clip(np.maximum(dst[..., 3], np.clip(add.max(axis=2), 0, 1)), 0, 1)


def flatten(fg, bg_rgb):
    """Composite a premultiplied layer over an opaque RGB background."""
    rgb = np.clip(fg[..., :3], 0, 1) + bg_rgb * (1 - np.clip(fg[..., 3], 0, 1))[..., None]
    return np.clip(rgb, 0, 1)


def to_image_rgb(a):
    return Image.fromarray((np.clip(a, 0, 1) * 255 + 0.5).astype(np.uint8), "RGB")


def to_image_rgba_premult(p):
    """Premultiplied float RGBA to a straight-alpha PIL image."""
    a = np.clip(p[..., 3], 0, 1)
    rgb = np.where(a[..., None] > 1e-4, p[..., :3] / np.maximum(a[..., None], 1e-4), 0)
    out = np.dstack([np.clip(rgb, 0, 1), a])
    return Image.fromarray((out * 255 + 0.5).astype(np.uint8), "RGBA")


def shade_mask(m, light=(-0.55, -0.8), soft_dp=2.6, bulge=1.0):
    """A pillowy 0..1 lighting term for the shape in mask `m` (lit from `light`)."""
    b = blur(m, px(soft_dp))
    gy, gx = np.gradient(b)
    nx, ny = -gx * px(soft_dp) * 2.2 * bulge, -gy * px(soft_dp) * 2.2 * bulge
    nz = np.ones_like(nx)
    inv = 1 / np.sqrt(nx * nx + ny * ny + nz * nz)
    lx, ly = light
    lz = 0.7
    ln = math.sqrt(lx * lx + ly * ly + lz * lz)
    dot = (nx * lx + ny * ly + nz * lz) * inv / ln
    return np.clip(dot, 0, 1)


# ── the ostrich ──────────────────────────────────────────────────────────────
# Facing right, head forward and a little low, cocky. The head/neck are authored in
# "design" coordinates and shown through a view (scale S, eye pinned to EYE_CANVAS);
# the plumage mound and dust live in canvas dp (y down, 108 dp square).

EYE = np.array([65.2, 34.6])           # design
FIG_S = 1.45
EYE_CANVAS = (52.5, 41.0)
NECK_PTS = bez((47.0, 100.0), (36.0, 80.0), (70.0, 78.0), (57.8, 40.5), n=260)


def figure_view():
    return use_view(FIG_S, EYE, EYE_CANVAS)


def canvas_view():
    return use_view(1.0, (0, 0), (0, 0))


def neck_radii():
    t = np.linspace(0, 1, len(NECK_PTS))
    return lerp(6.6, 3.9, smoothstep(0.0, 1.0, t) ** 0.8) + 0.5 * np.sin(math.pi * t)


def head_masks():
    head = m_ellipse((62.9, 35.4), 9.9, 5.3, 8)
    nape = m_ellipse((58.4, 39.2), 5.0, 4.8, 0)
    neck = m_discs(NECK_PTS, neck_radii())
    body = soften(np.maximum.reduce([head, nape, neck]), 1.0)
    # Beak: broad and flat, drooping a touch, rounded tip.
    top = bez((68.0, 31.9), (73.5, 31.3), (79.8, 33.4), (84.0, 37.8), n=60)
    tip = bez((84.0, 37.8), (85.6, 39.4), (85.0, 41.6), (82.9, 41.9), n=24)
    bot = bez((82.9, 41.9), (78.5, 43.6), (72.5, 43.4), (66.8, 41.4), n=60)
    beak = soften(m_poly(np.concatenate([top, tip, bot])), 0.5)
    return body, beak


def mouth_line():
    """The smirk: from the beak tip back under the eye, curling up at the end."""
    return bez((83.2, 40.9), (77.5, 42.8), (71.0, 42.4), (66.2, 39.4), n=80)


def lashes():
    """Ostrich lashes, fanning up and back from the top of the eye."""
    out = []
    for ang, ln in ((-140, 3.4), (-122, 4.4), (-104, 4.6), (-86, 3.8)):
        a = math.radians(ang)
        base = EYE + 4.0 * np.array([math.cos(a), math.sin(a)])
        tip = EYE + (4.0 + ln) * np.array([math.cos(a - 0.14), math.sin(a - 0.14)])
        out.append(tapered_stroke(np.linspace(base, tip, 18), 1.4, 0.4))
    return np.maximum.reduce(out)


def tuft():
    """A cheeky crest of fluff on top of the head: short wisps swept back."""
    out = []
    for k, (ang, ln) in enumerate(((-150, 4.6), (-132, 6.0), (-114, 6.6), (-96, 6.0), (-78, 4.8))):
        a = math.radians(ang)
        base = np.array([60.4 + 0.7 * k, 30.6])
        d = np.array([math.cos(a), math.sin(a)])
        ctrl = base + d * ln * 0.7
        tip = base + np.array([math.cos(a - 0.5), math.sin(a - 0.5)]) * ln
        pts = bez(base, base + d * ln * 0.3, ctrl, tip, n=40)
        out.append(tapered_stroke(pts, 2.2, 0.5))
    return np.maximum.reduce(out)


def neck_x_at_canvas_y(cy):
    s, ax, ay = _VIEW
    # NECK_PTS are design coordinates; find the one nearest canvas row cy.
    ys = ay + FIG_S * NECK_PTS[:, 1]
    k = int(np.argmin(np.abs(ys - cy)))
    return ax + FIG_S * NECK_PTS[k, 0]


def feather_mass():
    """The dark body plumage at the base of the neck: a scalloped, fluffy mound.
    Canvas coordinates."""
    with figure_view():
        cx = neck_x_at_canvas_y(86.0)
    with canvas_view():
        blobs = []
        rng = np.random.default_rng(7)
        for i in range(10):
            x = cx - 30 + i * 5.5
            y = 101 - 21.0 * math.exp(-((x - cx) / 15) ** 2) + rng.uniform(-1.0, 1.0)
            blobs.append(m_ellipse((x, y), rng.uniform(5.6, 7.4), rng.uniform(5.4, 6.8), rng.uniform(-30, 30)))
        for i in range(7):
            x = cx - 17 + i * 5.2
            y = 93 - 16.0 * math.exp(-((x - cx) / 11) ** 2)
            blobs.append(m_ellipse((x, y), 5.4, 4.8, -20 + i * 7))
        blobs.append(m_poly([(cx - 36, 112), (cx - 36, 104), (cx - 5, 100), (cx + 24, 104), (cx + 28, 112)]))
        return np.maximum.reduce(blobs)


def dust_clouds():
    """Cartoon dust trail behind the running ostrich (canvas dp): speed streaks
    sweeping in from the left, and round puffs at the feet. Returns (puffs, streaks)."""
    with canvas_view():
        puffs = [m_circle(c, r) for c, r in [((15, 93), 5.6), ((22, 97), 5.0), ((9, 90), 3.8), ((66, 96), 5.4), ((74, 94), 4.2), ((60, 99), 4.4)]]
        puffs = soften(np.maximum.reduce(puffs), 0.9)
        streaks = []
        for p0, p1, p2, p3, w in [
            ((-14, 49), (6, 45), (24, 47), (46, 55), 2.8),
            ((-14, 61), (8, 57), (28, 60), (52, 70), 3.6),
            ((-14, 73), (10, 70), (32, 75), (58, 86), 2.4),
        ]:
            pts = bez(p0, p1, p2, p3, n=200)
            streaks.append(leaf_stroke(pts, w))
        return puffs, np.maximum.reduce(streaks)


def beam_paths():
    """Two diverging laser beams (start, end, width_dp) in design coordinates."""
    out = []
    for ang, width in ((-8.0, 3.2), (-21.0, 2.3)):
        a = math.radians(ang)
        d = np.array([math.cos(a), math.sin(a)])
        out.append((EYE + d * 2.0, EYE + d * 110.0, width))
    return out


def beam_mask(width_scale=1.0, taper=False):
    ms = []
    for p0, p1, w in beam_paths():
        pts = np.linspace(p0, p1, 700)
        w1 = w * 0.6 if taper else w
        ms.append(tapered_stroke(pts, w * width_scale, w1 * width_scale))
    return np.maximum.reduce(ms)


def render_ostrich(include_beams=True, include_dust=True, include_feathers=True):
    """The foreground figure as a premultiplied RGBA layer (transparent outside)."""
    fg = layer()

    mound = feather_mass() if include_feathers else np.zeros((MASTER, MASTER), dtype=np.float32)

    # ---- canvas frame: dust trail behind everything -------------------------
    if include_dust:
        with canvas_view():
            cxx, cyy = grid()
            puffs, streaks = dust_clouds()
            puffs = puffs * 0.0   # the icon keeps only the speed streaks (puffs read as bones when cropped)
            over(fg, INK, blur(np.maximum(puffs, streaks), px(2.0)) * 0.25)
            # Streaks: bright cream with a warm lower edge.
            fade = smoothstep(-6, 30, cxx) * 0.85 + 0.1
            over(fg, lerp(SAND, OFFWHITE, 0.5), streaks * fade)
            # Puffs: lit from the upper right (the sun), plum shadow under.
            ps = shade_mask(puffs, light=(0.4, -0.85), soft_dp=3.0, bulge=1.5)
            pcol = ramp(ps, [(0, lerp(PLUM, SAND, 0.35)), (0.5, lerp(SAND, OFFWHITE, 0.25)), (1.0, OFFWHITE)])
            over(fg, INK, dilate(puffs, 0.45) * 0.55)
            over(fg, pcol, puffs * 0.97)

    # ---- design frame: the ostrich ------------------------------------------
    with figure_view():
        xx, yy = grid()
        body, beak = head_masks()
        tf = tuft()
        silhouette = np.maximum.reduce([body, beak, tf, mound])

        # Sticker shadow + outline.
        outline = dilate(silhouette, 1.5)
        off = (int(px(1.4)), int(px(0.5)))
        shadow = blur(shift(outline, off[0], off[1]), px(2.2))
        over(fg, INK, shadow * 0.5)
        over(fg, INK, outline)

        # Feather mound: deep plum-black with a warm sunset rim.
        if include_feathers:
            f_shade = shade_mask(mound, soft_dp=3.2, bulge=1.4)
            over(fg, ramp(f_shade, [(0, INK), (0.55, lerp(INK, PLUM, 0.5)), (1, lerp(PLUM, SKIN_SHADE, 0.5))]), mound)
            edge = np.clip(mound - blur(mound, px(1.5)), 0, 1)
            over(fg, lerp(BTC, BTC_HI, 0.4), edge * 0.5)

        # Crest of fluff (behind the skin).
        over(fg, INK, dilate(tf, 0.7))
        over(fg, ramp(shade_mask(tf, soft_dp=0.8), [(0, hexc('#4A1D5E')), (1, hexc('#A064A8'))]), tf)

        # Neck and head skin.
        shade = np.clip((shade_mask(body, soft_dp=3.6, bulge=1.9) - 0.15) / 0.8, 0, 1)
        base = ramp(shade, [(0.0, hexc('#8E4A74')), (0.4, lerp(SKIN_SHADE, SKIN, 0.7)), (0.75, SKIN), (1.0, OFFWHITE)])
        over(fg, base, body)
        # Sun rim on the right edge.
        rim = np.clip(body - blur(shift(body, 0, -int(px(1.3))), px(1.1)), 0, 1)
        over(fg, lerp(BTC, BTC_HI, 0.5), rim * 0.55)
        # Sparse feather flecks down the neck.
        rng = np.random.default_rng(11)
        flecks = []
        for _ in range(12):
            c = NECK_PTS[rng.integers(40, 200)] + rng.uniform(-1.2, 1.2, 2)
            flecks.append(m_ellipse(c, rng.uniform(0.45, 0.8), rng.uniform(1.0, 1.6), rng.uniform(-60, 60)))
        over(fg, lerp(SKIN, SKIN_SHADE, 0.55), np.maximum.reduce(flecks) * body * 0.5)

        # Beak.
        bshade = shade_mask(beak, light=(-0.35, -0.9), soft_dp=1.6, bulge=1.3)
        bt = np.clip((yy - 31) / 13.0, 0, 1)
        bcol = ramp(bshade * 0.65 + (1 - bt) * 0.35, [(0, hexc("#B94A0C")), (0.5, BTC), (1.0, BTC_HI)])
        over(fg, INK, dilate(beak, 0.5))
        over(fg, bcol, beak)
        over(fg, INK, tapered_stroke(mouth_line(), 0.8, 1.6, ease=1.2) * 0.9)
        over(fg, INK, m_ellipse((75.4, 35.6), 1.6, 0.6, 14) * 0.55)
        sheen = m_poly(np.concatenate([bez((69.5, 32.8), (73.5, 32.4), (78, 34.4), (81.4, 37.6), n=30), bez((80.6, 37.8), (77, 35.8), (72.5, 34.4), (69.5, 34.2), n=30)]))
        over(fg, OFFWHITE, sheen * 0.55 * beak)

        # The eye: a big hot socket, heavy cocky lid, lashes.
        eye_outer = m_ellipse(EYE, 3.9, 3.8)
        over(fg, INK, dilate(eye_outer, 0.55))
        rr = np.clip(np.hypot(xx - EYE[0], yy - EYE[1]) / 3.8, 0, 1) ** 1.1
        ecol = ramp(rr, [(0, np.array([1, 1, 1], dtype=np.float32)), (0.16, LASER_HOT), (0.42, LASER), (1.0, hexc('#A60E28'))])
        over(fg, ecol, eye_outer)
        lid = m_poly([(EYE[0] - 5.4, EYE[1] - 5.6), (EYE[0] + 5.8, EYE[1] - 4.8), (EYE[0] + 5.6, EYE[1] - 0.6), (EYE[0] - 5.0, EYE[1] - 2.0)])
        lid = lid * dilate(eye_outer, 0.4)
        over(fg, lerp(SKIN, SKIN_SHADE, 0.3), lid)
        lid_edge = np.clip(lid - blur(shift(lid, int(px(0.8)), 0), px(0.25)), 0, 1)
        over(fg, INK, lid_edge)
        over(fg, INK, lashes())

        # Light: the eye glow and the laser beams (additive).
        if include_beams:
            r = np.hypot(xx - EYE[0], yy - EYE[1])
            add_light(fg, LASER, np.exp(-(r / 5.0) ** 2) * 0.22 * (r > 3.2) + np.exp(-(r / 1.5) ** 2) * 0.35)
            core, body_b, wide = beam_mask(0.34), beam_mask(1.0), beam_mask(2.6)
            fall = 1.0 - smoothstep(88.0, 104.0, xx) * 0.97
            white = np.array([1, 1, 1], dtype=np.float32)
            add_light(fg, LASER, blur(wide, px(2.8)) * 0.55 * fall)
            add_light(fg, LASER, blur(body_b, px(0.8)) * 1.25 * fall)
            add_light(fg, LASER_HOT, blur(core, px(0.35)) * 1.1 * fall)
            add_light(fg, white, core * 0.9 * fall)
            for ang in (14, 104):
                a = math.radians(ang)
                d = np.array([math.cos(a), math.sin(a)])
                pts = np.linspace(EYE - d * 6.5, EYE + d * 6.5, 40)
                fl = m_discs(pts, 0.28 * (1 - np.abs(np.linspace(-1, 1, 40))) + 0.05)
                add_light(fg, LASER_HOT, blur(fl, px(0.5)) * 0.9)
    return np.clip(fg, 0, None)


def figure_mask(include_beams=False):
    """Alpha-only silhouette for the themed (monochrome) icon: body + beak + crest +
    plumage with the eye punched out, plus the beams as bold bars."""
    mound = feather_mass()
    with figure_view():
        body, beak = head_masks()
        sil = np.maximum.reduce([body, beak, tuft(), mound])
        eye_hole = m_ellipse(EYE, 3.0, 2.9)
        out = sil * (1 - eye_hole)
        if include_beams:
            out = np.maximum(out, beam_mask(1.0, taper=True))
    return np.clip(out, 0, 1)
