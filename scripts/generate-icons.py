#!/usr/bin/env python3
"""Write the FileMan app icon SVG, PNG, and ICO.

The mark is the two panes and their rows. Linux keeps the Breeze tile:
square corners, lighter at the top, and a long shadow at 45°. Windows is a
Fluent mica tile with rounded corners and the accent blue. macOS is full-bleed
Liquid Glass; the system draws the squircle.
"""
from __future__ import annotations

import math
import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VIEW = 256

# Breeze lighting: lighter at the top, darker at the bottom.
BG_TOP = (0x3A, 0x4C, 0x64)
BG_BOT = (0x16, 0x1C, 0x2A)
PANE_L_TOP = (0x4E, 0x68, 0x8C)
PANE_L_BOT = (0x24, 0x34, 0x4E)
PANE_R_TOP = (0x42, 0x58, 0x78)
PANE_R_BOT = (0x1C, 0x2A, 0x40)
SPLIT_TOP = (0x7A, 0x96, 0xB8)
SPLIT_BOT = (0x3A, 0x50, 0x6C)
MARK_HI = (0xC5, 0xE2, 0xFF)
MARK_LO = (0x2A, 0x6A, 0xC8)
ROW_HI = (0xA4, 0xBC, 0xD8)
ROW_LO = (0x3A, 0x50, 0x6C)
SHADOW = (0x10, 0x14, 0x1C)
# Long shadow at 45° toward the bottom-right, in the same units as the view.
SHADOW_X = 34.0
SHADOW_Y = 34.0
SHADOW_ALPHA = 0.55
# Thin lip, not a wide bevel. A 48px icon keeps this under one pixel.
RIM = 2.0
RIM_LIGHT = 0.32
RIM_DARK = 0.35

# (rect, pane index, "mark" or "row"). Rects are (x0, y0, x1, y1).
# Panes meet the divider. A gap would fill with the long shadow.
FRAMED = {
    "plate": (8.0, 8.0, 248.0, 248.0),
    "panes": (
        (28.0, 44.0, 124.0, 212.0),
        (132.0, 44.0, 228.0, 212.0),
    ),
    "divider": (124.0, 44.0, 132.0, 212.0),
    "bars": (
        ((38.0, 64.0, 110.0, 78.0), 0, "mark"),
        ((38.0, 88.0, 90.0, 98.0), 0, "row"),
        ((146.0, 64.0, 218.0, 74.0), 1, "row"),
        ((146.0, 82.0, 206.0, 92.0), 1, "row"),
    ),
}
# Full-bleed. macOS applies the squircle, so there is no outer frame.
FULL = {
    "plate": (0.0, 0.0, 256.0, 256.0),
    "panes": (
        (16.0, 32.0, 124.0, 224.0),
        (132.0, 32.0, 240.0, 224.0),
    ),
    "divider": (124.0, 32.0, 132.0, 224.0),
    "bars": (
        ((28.0, 52.0, 108.0, 66.0), 0, "mark"),
        ((28.0, 76.0, 86.0, 86.0), 0, "row"),
        ((148.0, 52.0, 228.0, 62.0), 1, "row"),
        ((148.0, 70.0, 214.0, 80.0), 1, "row"),
    ),
}
PANE_COLORS = ((PANE_L_TOP, PANE_L_BOT), (PANE_R_TOP, PANE_R_BOT))
BAR_COLORS = {"mark": (MARK_HI, MARK_LO), "row": (ROW_HI, ROW_LO)}


def fmt(value: float) -> str:
    return f"{value:g}"


def hex_rgb(color: tuple[int, int, int]) -> str:
    return f"#{color[0]:02x}{color[1]:02x}{color[2]:02x}"


def rect_tag(box: tuple[float, float, float, float], fill: str) -> str:
    x0, y0, x1, y1 = box
    return (
        f'<rect x="{fmt(x0)}" y="{fmt(y0)}" width="{fmt(x1 - x0)}" height="{fmt(y1 - y0)}" '
        f'rx="0" fill="{fill}"/>'
    )


def paint_rects(rects: list[tuple[float, float, float, float]], fill: str, dx: float, dy: float) -> str:
    shift = "" if dx == 0.0 and dy == 0.0 else f' transform="translate({fmt(dx)} {fmt(dy)})"'
    body = "\n".join(f"        {rect_tag(box, fill)}" for box in rects)
    return f"      <g{shift}>\n{body}\n      </g>"


def shadow_defs(prefix: str, rects: list[tuple[float, float, float, float]]) -> tuple[str, str]:
    masks = []
    layers = []
    steps = 8
    for index in range(1, steps + 1):
        far = index / steps
        near = (index - 1) / steps
        fade = SHADOW_ALPHA * (1.0 - (far + near) * 0.5)
        masks.append(
            f"""\
    <mask id="{prefix}{index}" maskContentUnits="userSpaceOnUse">
      <rect width="256" height="256" fill="#000"/>
{paint_rects(rects, "#fff", SHADOW_X * far, SHADOW_Y * far)}
{paint_rects(rects, "#000", SHADOW_X * near, SHADOW_Y * near)}
    </mask>"""
        )
        layers.append(
            f'    <rect width="256" height="256" fill="{hex_rgb(SHADOW)}" opacity="{fmt(fade)}" mask="url(#{prefix}{index})"/>'
        )
    return "\n".join(masks), "\n".join(layers)


def svg_text(layout: dict, note: str | None = None) -> str:
    plate = layout["plate"]
    panes = list(layout["panes"])
    divider = layout["divider"]
    bars = layout["bars"]
    outer = panes + [divider]
    inner = [bar[0] for bar in bars]
    outer_masks, outer_layers = shadow_defs("cast", outer)
    inner_masks, inner_layers = shadow_defs("bar", inner)
    pane_fills = "\n".join(
        f"    {rect_tag(box, fill)}"
        for box, fill in zip(panes, ("url(#paneL)", "url(#paneR)"), strict=True)
    )
    bar_fills = "\n".join(
        f"    {rect_tag(box, 'url(#mark)' if kind == 'mark' else 'url(#row)')}"
        for box, _pane, kind in bars
    )
    x0, y0, x1, y1 = plate
    comment = f"  <!-- {note} -->\n" if note else ""
    return f"""\
<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">
{comment}  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{hex_rgb(BG_TOP)}"/>
      <stop offset="1" stop-color="{hex_rgb(BG_BOT)}"/>
    </linearGradient>
    <linearGradient id="paneL" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{hex_rgb(PANE_L_TOP)}"/>
      <stop offset="1" stop-color="{hex_rgb(PANE_L_BOT)}"/>
    </linearGradient>
    <linearGradient id="paneR" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{hex_rgb(PANE_R_TOP)}"/>
      <stop offset="1" stop-color="{hex_rgb(PANE_R_BOT)}"/>
    </linearGradient>
    <linearGradient id="split" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{hex_rgb(SPLIT_TOP)}"/>
      <stop offset="1" stop-color="{hex_rgb(SPLIT_BOT)}"/>
    </linearGradient>
    <linearGradient id="mark" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{hex_rgb(MARK_HI)}"/>
      <stop offset="1" stop-color="{hex_rgb(MARK_LO)}"/>
    </linearGradient>
    <linearGradient id="row" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{hex_rgb(ROW_HI)}"/>
      <stop offset="1" stop-color="{hex_rgb(ROW_LO)}"/>
    </linearGradient>
    <clipPath id="plate">
      {rect_tag(plate, "#fff")}
    </clipPath>
    <clipPath id="panes">
      {rect_tag(panes[0], "#fff")}
      {rect_tag(panes[1], "#fff")}
    </clipPath>
{outer_masks}
{inner_masks}
  </defs>
  {rect_tag(plate, "url(#bg)")}
  <g clip-path="url(#plate)">
{outer_layers}
    <rect x="{fmt(x0)}" y="{fmt(y0)}" width="{fmt(x1 - x0)}" height="{fmt(RIM)}" fill="#fff" opacity="{fmt(RIM_LIGHT)}"/>
    <rect x="{fmt(x0)}" y="{fmt(y1 - RIM)}" width="{fmt(x1 - x0)}" height="{fmt(RIM)}" fill="#000" opacity="{fmt(RIM_DARK)}"/>
{pane_fills}
    {rect_tag(divider, "url(#split)")}
    <g clip-path="url(#panes)">
{inner_layers}
    </g>
{bar_fills}
  </g>
</svg>
"""


def sd_box(px: float, py: float, x0: float, y0: float, x1: float, y1: float) -> float:
    dx = abs(px - (x0 + x1) * 0.5) - (x1 - x0) * 0.5
    dy = abs(py - (y0 + y1) * 0.5) - (y1 - y0) * 0.5
    return math.hypot(max(dx, 0.0), max(dy, 0.0)) + min(max(dx, dy), 0.0)


def coverage(signed_distance: float) -> float:
    if signed_distance >= 0.5:
        return 0.0
    if signed_distance <= -0.5:
        return 1.0
    return 0.5 - signed_distance


def lerp(a: tuple[int, int, int], b: tuple[int, int, int], t: float) -> tuple[float, float, float]:
    t = 0.0 if t < 0.0 else 1.0 if t > 1.0 else t
    return (
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    )


def over(pixel: list[float], red: float, green: float, blue: float, alpha: float) -> None:
    if alpha <= 0.0:
        return
    inverse = 1.0 - alpha
    pixel[0] = red * alpha + pixel[0] * inverse
    pixel[1] = green * alpha + pixel[1] * inverse
    pixel[2] = blue * alpha + pixel[2] * inverse
    pixel[3] = alpha + pixel[3] * inverse


def shadow_hit(
    px: float, py: float, rects: tuple[tuple[float, float, float, float], ...], length: float
) -> float | None:
    best: float | None = None
    for x0, y0, x1, y1 in rects:
        s_lo = px - x1
        down = py - y1
        if down > s_lo:
            s_lo = down
        if s_lo < 0.0:
            s_lo = 0.0
        if s_lo > length:
            continue
        s_hi = px - x0
        down = py - y0
        if down < s_hi:
            s_hi = down
        if s_hi > length:
            s_hi = length
        if s_lo <= s_hi and (best is None or s_lo < best):
            best = s_lo
    return best


def long_shadow(
    px: float, py: float, rects: tuple[tuple[float, float, float, float], ...], scale: float
) -> float:
    length = SHADOW_X
    half = 0.25 / scale
    total = 0.0
    for oy in (-half, half):
        for ox in (-half, half):
            distance = shadow_hit(px + ox, py + oy, rects, length)
            if distance is not None:
                total += SHADOW_ALPHA * (1.0 - distance / length)
    return total * 0.25


def render(size: int, layout: dict) -> bytes:
    scale = size / VIEW
    plate = layout["plate"]
    panes = layout["panes"]
    divider = layout["divider"]
    bars = layout["bars"]
    outer = panes + (divider,)
    bars_by_pane: tuple[tuple[tuple[float, float, float, float], ...], ...] = (
        tuple(box for box, pane, _kind in bars if pane == 0),
        tuple(box for box, pane, _kind in bars if pane == 1),
    )
    pixels = [[[0.0, 0.0, 0.0, 0.0] for _ in range(size)] for _ in range(size)]
    px0, py0, px1, py1 = plate
    for y in range(size):
        py = (y + 0.5) / scale
        row = pixels[y]
        plate_t = (py - py0) / (py1 - py0)
        plate_rgb = lerp(BG_TOP, BG_BOT, plate_t)
        for x in range(size):
            px = (x + 0.5) / scale
            pixel = row[x]
            plate_alpha = coverage(sd_box(px, py, *plate) * scale)
            if plate_alpha:
                over(pixel, *plate_rgb, plate_alpha)
                shade = long_shadow(px, py, outer, scale) * plate_alpha
                if shade:
                    over(pixel, float(SHADOW[0]), float(SHADOW[1]), float(SHADOW[2]), shade)
                lip = coverage((py0 - py) * scale) * coverage((py - (py0 + RIM)) * scale) * plate_alpha
                if lip:
                    over(pixel, 255.0, 255.0, 255.0, RIM_LIGHT * lip)
                foot = coverage(((py1 - RIM) - py) * scale) * coverage((py - py1) * scale) * plate_alpha
                if foot:
                    over(pixel, 0.0, 0.0, 0.0, RIM_DARK * foot)
            for box, (top, bot) in zip(panes, PANE_COLORS, strict=True):
                alpha = coverage(sd_box(px, py, *box) * scale)
                if alpha:
                    red, green, blue = lerp(top, bot, (py - box[1]) / (box[3] - box[1]))
                    over(pixel, red, green, blue, alpha)
            split_alpha = coverage(sd_box(px, py, *divider) * scale)
            if split_alpha:
                red, green, blue = lerp(SPLIT_TOP, SPLIT_BOT, (py - divider[1]) / (divider[3] - divider[1]))
                over(pixel, red, green, blue, split_alpha)
            for pane_box, pane_bars in zip(panes, bars_by_pane, strict=True):
                pane_alpha = coverage(sd_box(px, py, *pane_box) * scale)
                if pane_alpha:
                    shade = long_shadow(px, py, pane_bars, scale) * pane_alpha
                    if shade:
                        over(pixel, float(SHADOW[0]), float(SHADOW[1]), float(SHADOW[2]), shade)
            for box, _pane, kind in bars:
                alpha = coverage(sd_box(px, py, *box) * scale)
                if alpha:
                    top, bot = BAR_COLORS[kind]
                    red, green, blue = lerp(top, bot, (py - box[1]) / (box[3] - box[1]))
                    over(pixel, red, green, blue, alpha)
    out = bytearray(size * size * 4)
    i = 0
    for row in pixels:
        for red, green, blue, alpha in row:
            out[i] = int(red + 0.5)
            out[i + 1] = int(green + 0.5)
            out[i + 2] = int(blue + 0.5)
            out[i + 3] = int(alpha * 255.0 + 0.5)
            i += 4
    return bytes(out)


def png_bytes(width: int, height: int, rgba: bytes, rgb: bool = False) -> bytes:
    def chunk(tag: bytes, data: bytes) -> bytes:
        crc = zlib.crc32(tag + data) & 0xFFFFFFFF
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", crc)

    if rgb:
        raw_rows = []
        for y in range(height):
            row = bytearray(b"\x00")
            start = y * width * 4
            for x in range(width):
                i = start + x * 4
                row += rgba[i : i + 3]
            raw_rows.append(bytes(row))
        color = 2
    else:
        raw_rows = [b"\x00" + rgba[y * width * 4 : (y + 1) * width * 4] for y in range(height)]
        color = 6
    ihdr = struct.pack(">IIBBBBB", width, height, 8, color, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(b"".join(raw_rows), 9)) + chunk(b"IEND", b"")


def ico_bytes(images: list[tuple[int, bytes]]) -> bytes:
    count = len(images)
    offset = 6 + 16 * count
    header = struct.pack("<HHH", 0, 1, count)
    entries = bytearray()
    payload = bytearray()
    for size, data in images:
        stored = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", stored, stored, 0, 0, 1, 32, len(data), offset)
        payload += data
        offset += len(data)
    return bytes(header + entries + payload)


def pixel(rgba: bytes, size: int, x: int, y: int) -> tuple[int, int, int, int]:
    i = (y * size + x) * 4
    return rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]


def check_square(name: str, rgba: bytes, size: int, layout: dict) -> None:
    scale = size / VIEW
    def at(vx: float, vy: float) -> tuple[int, int, int, int]:
        return pixel(rgba, size, int(vx * scale), int(vy * scale))

    plate = layout["plate"]
    # The plate corner is the pixel on the corner, and the pixel just outside is clear.
    corner = at(plate[0], plate[1])
    if corner[3] != 255:
        raise SystemExit(f"{name}: plate corner is not opaque {corner}")
    if plate[0] > 0.0:
        outside = at(plate[0] - 1.0, plate[1] - 1.0)
        if outside[3] != 0:
            raise SystemExit(f"{name}: inset outside the plate is not clear {outside}")
    # A pane corner that used to sit outside the rounded rect is now filled.
    pane = layout["panes"][0]
    pane_px = at(pane[0], pane[1])
    plate_px = at(max(plate[0], pane[0] - 12.0), pane[1])
    if sum(pane_px[:3]) < sum(plate_px[:3]) + 40:
        raise SystemExit(f"{name}: pane corner is not square and lit {pane_px} vs {plate_px}")
    # The selected row's corner is the accent, so its old pill radius is gone.
    bar = layout["bars"][0][0]
    accent = at(bar[0], bar[1])
    if accent[2] < 180 or accent[2] < accent[0]:
        raise SystemExit(f"{name}: selection corner is not the lit accent {accent}")
    # Shadow falls down-right of that row, inside the pane and off the row itself.
    lit = at(pane[0] + 4.0, bar[3] + 4.0)
    cast = at((bar[0] + bar[2]) * 0.5, bar[3] + 4.0)
    if sum(cast[:3]) > sum(lit[:3]) - 12:
        raise SystemExit(f"{name}: selection does not cast a long shadow {cast} vs {lit}")
    top = at((plate[0] + plate[2]) * 0.5, plate[1] + 8.0)
    below = at((plate[0] + plate[2]) * 0.5, plate[1] + 28.0)
    if top[1] <= below[1]:
        raise SystemExit(f"{name}: plate is not lighter at the top {top} vs {below}")


# Windows 11 Fluent. Rounded mica tile, accent blue, shadow falls straight down.
WIN_PLATE = (10.0, 8.0, 246.0, 234.0)
WIN_PLATE_R = 44.0
WIN_PANE_R = 14.0
WIN_BG_TOP = (0x3E, 0x44, 0x4C)
WIN_BG_BOT = (0x1A, 0x1C, 0x20)
WIN_PANE_COLORS = (
    ((0x5C, 0x64, 0x70), (0x2E, 0x34, 0x3C)),
    ((0x4E, 0x56, 0x62), (0x26, 0x2C, 0x34)),
)
WIN_SPLIT_TOP = (0x9A, 0xA4, 0xB2)
WIN_SPLIT_BOT = (0x52, 0x5A, 0x66)
WIN_MARK_HI = (0xE4, 0xF6, 0xFF)
WIN_MARK_LO = (0x00, 0x78, 0xD4)
WIN_ROW_HI = (0xE2, 0xE6, 0xEC)
WIN_ROW_LO = (0x74, 0x7C, 0x88)

# Liquid Glass. Light comes from above. Panes are translucent with a lens edge.
GLASS_PANE_R = 28.0
GLASS_BG_TOP = (0x9E, 0xC4, 0xE6)
GLASS_BG_BOT = (0x16, 0x28, 0x44)
GLASS_MARK_HI = (0xB4, 0xE4, 0xFF)
GLASS_MARK_LO = (0x00, 0x6E, 0xE0)
GLASS_GLOW = (0x64, 0xC2, 0xFF)


def sd_round_box(
    px: float, py: float, x0: float, y0: float, x1: float, y1: float, radius: float
) -> float:
    half_w = (x1 - x0) * 0.5
    half_h = (y1 - y0) * 0.5
    radius = min(radius, half_w, half_h)
    dx = abs(px - (x0 + half_w)) - (half_w - radius)
    dy = abs(py - (y0 + half_h)) - (half_h - radius)
    return math.hypot(max(dx, 0.0), max(dy, 0.0)) + min(max(dx, dy), 0.0) - radius


def pack_rgba(pixels: list[list[list[float]]]) -> bytes:
    out = bytearray(len(pixels) * len(pixels[0]) * 4)
    i = 0
    for row in pixels:
        for red, green, blue, alpha in row:
            out[i] = int(red + 0.5)
            out[i + 1] = int(green + 0.5)
            out[i + 2] = int(blue + 0.5)
            out[i + 3] = int(alpha * 255.0 + 0.5)
            i += 4
    return bytes(out)


def render_windows(size: int, layout: dict) -> bytes:
    scale = size / VIEW
    plate = WIN_PLATE
    panes = layout["panes"]
    divider = layout["divider"]
    bars = layout["bars"]
    pixels = [[[0.0, 0.0, 0.0, 0.0] for _ in range(size)] for _ in range(size)]
    px0, py0, px1, py1 = plate
    for y in range(size):
        py = (y + 0.5) / scale
        row = pixels[y]
        plate_rgb = lerp(WIN_BG_TOP, WIN_BG_BOT, (py - py0) / (py1 - py0))
        for x in range(size):
            px = (x + 0.5) / scale
            pixel = row[x]
            for dy, alpha in ((4.0, 0.22), (9.0, 0.16), (15.0, 0.10)):
                shade = coverage(sd_round_box(px, py - dy, *plate, WIN_PLATE_R) * scale)
                if shade:
                    over(pixel, 0.0, 0.0, 0.0, shade * alpha)
            plate_sd = sd_round_box(px, py, *plate, WIN_PLATE_R)
            plate_alpha = coverage(plate_sd * scale)
            if plate_alpha:
                over(pixel, *plate_rgb, plate_alpha)
                if -1.6 < plate_sd < 0.4:
                    edge = 1.0 - abs(plate_sd + 0.6) / 1.0
                    if edge > 0.0:
                        over(pixel, 255.0, 255.0, 255.0, 0.28 * edge * plate_alpha)
                sheen_t = (py - py0) / (py1 - py0)
                if sheen_t < 0.22:
                    over(pixel, 255.0, 255.0, 255.0, 0.16 * (1.0 - sheen_t / 0.22) * plate_alpha)
            for box, (top, bot) in zip(panes, WIN_PANE_COLORS, strict=True):
                alpha = coverage(sd_round_box(px, py, *box, WIN_PANE_R) * scale)
                if alpha:
                    red, green, blue = lerp(top, bot, (py - box[1]) / (box[3] - box[1]))
                    over(pixel, red, green, blue, alpha)
            split_alpha = coverage(sd_round_box(px, py, *divider, 4.0) * scale)
            if split_alpha:
                red, green, blue = lerp(WIN_SPLIT_TOP, WIN_SPLIT_BOT, (py - divider[1]) / (divider[3] - divider[1]))
                over(pixel, red, green, blue, split_alpha)
            for box, _pane, kind in bars:
                radius = (box[3] - box[1]) * 0.5
                alpha = coverage(sd_round_box(px, py, *box, radius) * scale)
                if not alpha:
                    continue
                top, bot = (WIN_MARK_HI, WIN_MARK_LO) if kind == "mark" else (WIN_ROW_HI, WIN_ROW_LO)
                red, green, blue = lerp(top, bot, (py - box[1]) / (box[3] - box[1]))
                over(pixel, red, green, blue, alpha)
    return pack_rgba(pixels)


def glass_background(px: float, py: float) -> tuple[float, float, float]:
    base = lerp(GLASS_BG_TOP, GLASS_BG_BOT, (py / VIEW) ** 0.9)
    dx = (px - 128.0) / 108.0
    dy = (py - 24.0) / 88.0
    pool = max(0.0, 1.0 - (dx * dx * 0.7 + dy * dy))
    return (
        base[0] + (255.0 - base[0]) * pool * 0.62,
        base[1] + (255.0 - base[1]) * pool * 0.55,
        base[2] + (255.0 - base[2]) * pool * 0.38,
    )


def render_glass(size: int, layout: dict) -> bytes:
    scale = size / VIEW
    panes = layout["panes"]
    divider = layout["divider"]
    bars = layout["bars"]
    pixels = [[[0.0, 0.0, 0.0, 0.0] for _ in range(size)] for _ in range(size)]
    for y in range(size):
        py = (y + 0.5) / scale
        row = pixels[y]
        for x in range(size):
            px = (x + 0.5) / scale
            pixel = row[x]
            over(pixel, *glass_background(px, py), 1.0)
            if py < 42.0:
                over(pixel, 255.0, 255.0, 255.0, 0.48 * (1.0 - py / 42.0) ** 1.35)
            for box in panes:
                for dy, alpha in ((20.0, 0.10), (12.0, 0.16), (6.0, 0.18)):
                    shade = coverage(sd_round_box(px, py - dy, *box, GLASS_PANE_R) * scale * 0.45)
                    if shade:
                        over(pixel, 6.0, 16.0, 36.0, shade * alpha)
            for box in panes:
                sd = sd_round_box(px, py, *box, GLASS_PANE_R)
                cov = coverage(sd * scale)
                if not cov:
                    continue
                t = (py - box[1]) / (box[3] - box[1])
                frost = lerp((236, 246, 255), (120, 156, 190), min(1.0, t * 1.15))
                over(pixel, frost[0], frost[1], frost[2], (0.30 * (1.0 - t) + 0.10) * cov)
                if t < 0.09:
                    spec = (1.0 - t / 0.09) ** 1.2
                    over(pixel, 255.0, 255.0, 255.0, 0.78 * spec * cov)
                if -3.0 < sd < 0.8:
                    closeness = 1.0 - abs(sd + 1.0) / 1.9
                    if closeness > 0.0:
                        strength = closeness * (0.30 + 0.70 * max(0.0, 1.0 - t))
                        over(pixel, 255.0, 255.0, 255.0, 0.90 * strength * cov)
                        if t > 0.62:
                            over(pixel, 8.0, 20.0, 42.0, 0.40 * closeness * (t - 0.62) / 0.38 * cov)
            split = coverage(sd_round_box(px, py, *divider, 4.0) * scale)
            if split:
                over(pixel, 255.0, 255.0, 255.0, 0.42 * split)
            for box, _pane, kind in bars:
                height = box[3] - box[1]
                radius = height * 0.5
                if kind == "mark":
                    pad = 8.0
                    glow = coverage(
                        sd_round_box(px, py, box[0] - pad, box[1] - pad, box[2] + pad, box[3] + pad, radius + pad)
                        * scale
                        * 0.45
                    )
                    if glow:
                        over(pixel, float(GLASS_GLOW[0]), float(GLASS_GLOW[1]), float(GLASS_GLOW[2]), 0.55 * glow)
                alpha = coverage(sd_round_box(px, py, *box, radius) * scale)
                if not alpha:
                    continue
                t = (py - box[1]) / height
                if kind == "mark":
                    red, green, blue = lerp(GLASS_MARK_HI, GLASS_MARK_LO, t)
                    over(pixel, red, green, blue, alpha)
                    if t < 0.30:
                        over(pixel, 255.0, 255.0, 255.0, 0.72 * (1.0 - t / 0.30) * alpha)
                else:
                    over(pixel, 255.0, 255.0, 255.0, (0.16 + 0.28 * max(0.0, 1.0 - t)) * alpha)
    return pack_rgba(pixels)


def glass_svg(layout: dict) -> str:
    panes = layout["panes"]
    divider = layout["divider"]
    bars = layout["bars"]

    def rounded(box: tuple[float, float, float, float], radius: float, fill: str, extra: str = "") -> str:
        x0, y0, x1, y1 = box
        return (
            f'<rect x="{fmt(x0)}" y="{fmt(y0)}" width="{fmt(x1 - x0)}" height="{fmt(y1 - y0)}" '
            f'rx="{fmt(radius)}" fill="{fill}"{extra}/>'
        )

    pane_tags = "\n    ".join(rounded(box, GLASS_PANE_R, "url(#frost)", ' stroke="#fff" stroke-opacity="0.8" stroke-width="2"') for box in panes)
    bar_tags = []
    glow = ""
    for box, _pane, kind in bars:
        radius = (box[3] - box[1]) * 0.5
        if kind == "mark":
            pad = 8.0
            glow = rounded(
                (box[0] - pad, box[1] - pad, box[2] + pad, box[3] + pad),
                radius + pad,
                "#64c2ff",
                ' opacity="0.7" filter="url(#glow)"',
            )
            bar_tags.append(rounded(box, radius, "url(#mark)"))
        else:
            bar_tags.append(rounded(box, radius, "url(#row)"))
    x0, y0, x1, y1 = divider
    seam = (
        f'<rect x="{fmt(x0)}" y="{fmt(y0)}" width="{fmt(x1 - x0)}" height="{fmt(y1 - y0)}" '
        f'rx="4" fill="#fff" opacity="0.42"/>'
    )
    return f"""\
<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">
  <!-- Full-bleed Liquid Glass. macOS applies the squircle. -->
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{hex_rgb(GLASS_BG_TOP)}"/>
      <stop offset="1" stop-color="{hex_rgb(GLASS_BG_BOT)}"/>
    </linearGradient>
    <radialGradient id="pool" cx="50%" cy="8%" r="58%">
      <stop offset="0" stop-color="#ffffff" stop-opacity="0.9"/>
      <stop offset="1" stop-color="#ffffff" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="lip" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#ffffff" stop-opacity="0.55"/>
      <stop offset="1" stop-color="#ffffff" stop-opacity="0"/>
    </linearGradient>
    <linearGradient id="frost" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#ffffff" stop-opacity="0.85"/>
      <stop offset="0.1" stop-color="#ffffff" stop-opacity="0.28"/>
      <stop offset="1" stop-color="#78a0be" stop-opacity="0.16"/>
    </linearGradient>
    <linearGradient id="mark" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{hex_rgb(GLASS_MARK_HI)}"/>
      <stop offset="1" stop-color="{hex_rgb(GLASS_MARK_LO)}"/>
    </linearGradient>
    <linearGradient id="row" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#ffffff" stop-opacity="0.62"/>
      <stop offset="1" stop-color="#ffffff" stop-opacity="0.16"/>
    </linearGradient>
    <filter id="glow" x="-50%" y="-50%" width="200%" height="200%">
      <feGaussianBlur stdDeviation="5"/>
    </filter>
  </defs>
  <rect width="256" height="256" fill="url(#bg)"/>
  <rect width="256" height="256" fill="url(#pool)"/>
  <rect width="256" height="46" fill="url(#lip)"/>
  {pane_tags}
  {seam}
  {glow}
  {"    " + chr(10) + "    ".join(bar_tags)}
</svg>
"""


def check_windows(rgba: bytes, size: int) -> None:
    scale = size / VIEW

    def at(vx: float, vy: float) -> tuple[int, int, int, int]:
        return pixel(rgba, size, min(size - 1, int(vx * scale)), min(size - 1, int(vy * scale)))

    # The plate's square corner is outside the rounded Fluent tile.
    if at(WIN_PLATE[0], WIN_PLATE[1])[3] != 0:
        raise SystemExit(f"windows: plate corner is not rounded {at(WIN_PLATE[0], WIN_PLATE[1])}")
    center_top = at((WIN_PLATE[0] + WIN_PLATE[2]) * 0.5, WIN_PLATE[1] + 6.0)
    if center_top[3] != 255:
        raise SystemExit(f"windows: tile face is not opaque {center_top}")
    mark = FRAMED["bars"][0][0]
    accent = at((mark[0] + mark[2]) * 0.5, (mark[1] + mark[3]) * 0.5)
    if accent[2] < accent[0] + 30 or accent[2] < 160:
        raise SystemExit(f"windows: selection is not the accent blue {accent}")


def check_glass(name: str, rgba: bytes, size: int) -> None:
    scale = size / VIEW

    def at(vx: float, vy: float) -> tuple[int, int, int, int]:
        return pixel(rgba, size, min(size - 1, int(vx * scale)), min(size - 1, int(vy * scale)))

    if at(1.0, 1.0)[3] != 255:
        raise SystemExit(f"{name}: macOS icon is not full-bleed")
    pane = FULL["panes"][0]
    top = at((pane[0] + pane[2]) * 0.5, pane[1] + 6.0)
    bot = at((pane[0] + pane[2]) * 0.5, pane[3] - 10.0)
    if sum(top[:3]) < sum(bot[:3]) + 25:
        raise SystemExit(f"{name}: glass pane is not brighter at the top {top} vs {bot}")
    mark = FULL["bars"][0][0]
    accent = at((mark[0] + mark[2]) * 0.5, (mark[1] + mark[3]) * 0.5)
    beside = at(mark[2] + 10.0, (mark[1] + mark[3]) * 0.5)
    if accent[2] < accent[0] + 50 or accent[2] < beside[2] + 20:
        raise SystemExit(f"{name}: selection does not glow {accent} vs {beside}")


def main() -> None:
    macos = ROOT / "etc" / "macos"
    windows = ROOT / "etc" / "windows"
    macos.mkdir(parents=True, exist_ok=True)
    windows.mkdir(parents=True, exist_ok=True)
    framed_256 = render(256, FRAMED)
    check_square("linux", framed_256, 256, FRAMED)
    windows_256 = render_windows(256, FRAMED)
    check_windows(windows_256, 256)
    glass_256 = render_glass(256, FULL)
    check_glass("glass", glass_256, 256)
    glass_1024 = render_glass(1024, FULL)
    check_glass("glass-1024", glass_1024, 1024)
    if any(glass_1024[i] != 255 for i in range(3, len(glass_1024), 4)):
        raise SystemExit("macOS icon is not full-bleed opaque")
    (ROOT / "etc" / "fileman.svg").write_text(svg_text(FRAMED), encoding="utf-8")
    (macos / "icon.svg").write_text(glass_svg(FULL), encoding="utf-8")
    (macos / "icon.png").write_bytes(png_bytes(1024, 1024, glass_1024, rgb=True))
    ico_sizes = (16, 24, 32, 48, 64, 128, 256)
    ico_images = []
    for size in ico_sizes:
        rgba = windows_256 if size == 256 else render_windows(size, FRAMED)
        ico_images.append((size, png_bytes(size, size, rgba)))
    (windows / "icon.ico").write_bytes(ico_bytes(ico_images))
    print("wrote etc/fileman.svg, etc/macos/icon.svg, etc/macos/icon.png, etc/windows/icon.ico")


if __name__ == "__main__":
    main()
