"""Draws Quickseek's icon (concept B: a lens with speed lines) and writes:

  assets/quickseek.ico           16-256 px (100-200 % display scaling), each
                                 size drawn on the pixel grid for itself
  docs/images/icon-sizes.png     contact sheet of every size, light and dark
  docs/images/icon.svg           the concept, teal for light backgrounds
  docs/images/icon-dark.svg      lighter teal, for dark backgrounds
  docs/images/icon.png, icon-dark.png          256 px
  docs/images/social-preview.png               1280x640 with the name (GitHub)

Every size keeps the speed lines: two at 16-20 px, three from 24 px, with
whole-pixel heights, at least 1 px apart and 2 px clear of the ring. The
.ico uses a mid teal that keeps 3.9:1 contrast on light and dark taskbars.

    python tools/make-icon.py
"""
import io
import os
import struct

from PIL import Image, ImageDraw, ImageFont

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
TEAL = (0x0F, 0x6E, 0x56)
TEAL_DARK = (0x5D, 0xCA, 0xA5)  # on dark backgrounds
# Every size in the .ico, drawn for itself: 16-64 px at 100-200 % display
# scaling (16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96) and 128, 256.
SIZES = [16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 128, 256]
# The .ico sits on light and dark taskbars and Explorer views alike: a mid
# teal with 3.9:1 on both #F3F3F3 and #1C1C1C (#0F6E56 has 2.75:1 on dark).
TEAL_SMALL = (0x16, 0x89, 0x6F)

# The concept, in a 100-unit box: the lens ring, its handle at 45 degrees,
# speed lines on the left.
LENS = (58, 40, 25)  # cx, cy, r (ring centre line)
RING = 12
HANDLE = ((77, 59), (93, 75), 14)
LINES = [  # x1, x2, y, opacity
    (6, 22, 28, 0.55),
    (2, 24, 42, 0.75),
    (10, 26, 56, 0.55),
]


def layout(size):
    """Whole-pixel geometry for `size`: the ring's box and stroke, the
    handle, and the speed lines as (x0, x1, y0, y1, opacity) rectangles.
    Two lines at 16-20 px, three from 24 px; each line ends at least
    `gap` px left of the ring at its rows, and lines are 1 px apart or more."""
    s = size
    stroke = max(2, round(0.11 * s))
    d = max(9, round(0.54 * s))  # ring's outer diameter
    left = round(0.65 * s - d / 2)
    top = round(0.42 * s - d / 2)
    cx, cy, r_out = left + d / 2, top + d / 2, d / 2
    handle_w = stroke + max(1, round(0.02 * s))
    reach = 0.30 * s
    handle = ((cx + (r_out - stroke / 2) * 0.7071, cy + (r_out - stroke / 2) * 0.7071),
              (cx + reach, cy + reach), handle_w)
    n = 2 if s <= 20 else 3
    h = max(2, round(0.075 * s))
    g = max(2 if s <= 20 else 1, round(0.06 * s))
    gap = max(2, round(0.07 * s))
    total = n * h + (n - 1) * g
    y = round(cy - total / 2)
    # Where each line starts, as a share of the room left of the ring
    # (the concept's lines: top 6-22, middle 2-24, bottom 10-26).
    room = left - gap
    if n == 2:
        starts, alphas = [0.0, 0.3], [1.0, 0.65]
    else:
        starts = [0.25, 0.0, 0.4]
        alphas = [0.6, 1.0, 0.6] if s < 64 else [0.55, 0.8, 0.55]
    starts = [round(f * room) for f in starts]
    lines = []
    for i in range(n):
        y0, y1 = y, y + h
        # The ring's outer left edge over these rows (nearest to the line).
        dy = max(0.0, min(abs(y0 - cy), abs(y1 - cy)) if not (y0 <= cy <= y1) else 0.0)
        edge = cx - (r_out ** 2 - min(dy, r_out) ** 2) ** 0.5
        x1 = int(edge - gap)
        x0 = min(starts[i], x1 - 2)
        lines.append((max(0, x0), x1, y0, y1, alphas[i]))
        y = y1 + g
    return (left, top, d, stroke), handle, lines


def draw(size, color=None):
    """The icon at `size` px, transparent background."""
    color = color or TEAL_SMALL
    ss = 16
    n = size * ss
    (left, top, d, stroke), (h0, h1, hw), lines = layout(size)
    shape = Image.new("L", (n, n), 0)
    sd = ImageDraw.Draw(shape)
    L, T, D, S = left * ss, top * ss, d * ss, stroke * ss
    sd.ellipse((L, T, L + D - 1, T + D - 1), fill=255)
    sd.ellipse((L + S, T + S, L + D - 1 - S, T + D - 1 - S), fill=0)
    (x0, y0), (x1, y1) = (h0[0] * ss, h0[1] * ss), (h1[0] * ss, h1[1] * ss)
    w = hw * ss
    sd.line((x0, y0, x1, y1), fill=255, width=int(w))
    sd.ellipse((x1 - w / 2, y1 - w / 2, x1 + w / 2, y1 + w / 2), fill=255)
    img = Image.new("RGBA", (n, n), color + (0,))
    img.putalpha(shape)
    for a, b, c, e, o in lines:
        layer = Image.new("L", (n, n), 0)
        ld = ImageDraw.Draw(layer)
        rad = (e - c) * ss / 2 if e - c >= 4 else 0
        ld.rounded_rectangle((a * ss, c * ss, b * ss - 1, e * ss - 1), radius=rad, fill=int(255 * o))
        line = Image.new("RGBA", (n, n), color + (0,))
        line.putalpha(layer)
        img = Image.alpha_composite(img, line)
    return img.resize((size, size), Image.BOX)


def draw_vector(size, color):
    """The concept itself (no pixel snapping), for the README PNGs."""
    ss = 8
    n = size * ss
    u = n / 100.0
    shape = Image.new("L", (n, n), 0)
    sd = ImageDraw.Draw(shape)
    cx, cy, r = (v * u for v in LENS)
    t = RING * u
    sd.ellipse((cx - r - t / 2, cy - r - t / 2, cx + r + t / 2, cy + r + t / 2), fill=255)
    sd.ellipse((cx - r + t / 2, cy - r + t / 2, cx + r - t / 2, cy + r - t / 2), fill=0)
    (x0, y0), (x1, y1), hw = HANDLE
    x0, y0, x1, y1, hw = x0 * u, y0 * u, x1 * u, y1 * u, hw * u
    sd.line((x0, y0, x1, y1), fill=255, width=int(hw))
    for x, y in ((x0, y0), (x1, y1)):
        sd.ellipse((x - hw / 2, y - hw / 2, x + hw / 2, y + hw / 2), fill=255)
    img = Image.new("RGBA", (n, n), color + (0,))
    img.putalpha(shape)
    for a, b, y, o in LINES:
        layer = Image.new("L", (n, n), 0)
        ld = ImageDraw.Draw(layer)
        w = 7 * u
        ld.rounded_rectangle((a * u, y * u - w / 2, b * u, y * u + w / 2), radius=w / 2, fill=int(255 * o))
        line = Image.new("RGBA", (n, n), color + (0,))
        line.putalpha(layer)
        img = Image.alpha_composite(img, line)
    return img.resize((size, size), Image.LANCZOS)


def contact_sheet(path):
    """Every size of the .ico at its real size and magnified 6x (nearest),
    on a light and a dark taskbar, with the 256 px design for reference."""
    small = [s for s in SIZES if s <= 64]
    zoom = 6
    pad = 16
    col_w = [max(s * zoom, 60) for s in small]
    w = sum(col_w) + pad * (len(small) + 1) + 256 + pad
    row_h = 64 * zoom + 40
    sheet = Image.new("RGBA", (w, 2 * row_h + 2 * pad + 30), (255, 255, 255, 255))
    d = ImageDraw.Draw(sheet)
    f = font(16, False)
    for row, bg in enumerate([(0xF3, 0xF3, 0xF3), (0x1C, 0x1C, 0x1C)]):
        y0 = pad + row * (row_h + pad)
        d.rectangle((0, y0 - 6, w, y0 + row_h - 6), fill=bg + (255,))
        ink = (0x20, 0x20, 0x20) if row == 0 else (0xE0, 0xE0, 0xE0)
        x = pad
        for s, cw in zip(small, col_w):
            im = draw(s)
            sheet.alpha_composite(im, (x, y0 + 4))
            big = im.resize((s * zoom, s * zoom), Image.NEAREST)
            sheet.alpha_composite(big, (x, y0 + 4 + 64 + 8))
            d.text((x, y0 + row_h - 32), f"{s}px", font=f, fill=ink)
            x += cw + pad
        ref = draw(256)
        sheet.alpha_composite(ref, (x, y0 + 4))
        d.text((x, y0 + row_h - 32), "256px", font=f, fill=ink)
    d.text((pad, 2 * row_h + 2 * pad), "Each size as drawn (top of each row) and magnified 6x; light and dark taskbar.", font=f, fill=(0x40, 0x40, 0x40))
    sheet.convert("RGB").save(path, optimize=True)


def write_ico(path, images):
    """An .ico with one PNG-compressed entry per image."""
    blobs = []
    for im in images:
        b = io.BytesIO()
        im.save(b, "PNG", optimize=True)
        blobs.append(b.getvalue())
    head = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries = b""
    for im, data in zip(images, blobs):
        w = im.width if im.width < 256 else 0
        entries += struct.pack("<BBBBHHII", w, w, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    with open(path, "wb") as f:
        f.write(head + entries + b"".join(blobs))


def svg(color):
    hexc = "#%02X%02X%02X" % color
    cx, cy, r = LENS
    (x0, y0), (x1, y1), hw = HANDLE
    lines = "".join(
        f'<line x1="{a}" y1="{y}" x2="{b}" y2="{y}" stroke-width="7" opacity="{o}"/>'
        for a, b, y, o in LINES
    )
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" width="256" height="256">'
        '<title>Quickseek</title>'
        f'<g fill="none" stroke="{hexc}" stroke-linecap="round">'
        f'<circle cx="{cx}" cy="{cy}" r="{r}" stroke-width="{RING}"/>'
        f'<line x1="{x0}" y1="{y0}" x2="{x1}" y2="{y1}" stroke-width="{hw}"/>'
        f"{lines}</g></svg>\n"
    )


def font(size, bold=True):
    for name in (["segoeuib.ttf", "seguisb.ttf"] if bold else ["segoeui.ttf"]):
        p = os.path.join(os.environ.get("WINDIR", r"C:\Windows"), "Fonts", name)
        if os.path.exists(p):
            return ImageFont.truetype(p, size)
    return ImageFont.load_default()


def social_preview(path):
    """GitHub's social preview: 1280x640, icon and name, a light card."""
    w, h = 1280, 640
    img = Image.new("RGB", (w, h), (0xF7, 0xF8, 0xF7))
    icon = draw_vector(300, TEAL)
    img.paste(icon, (140, (h - 300) // 2 - 10), icon)
    d = ImageDraw.Draw(img)
    d.text((480, 205), "Quickseek", font=font(132), fill=(0x1F, 0x24, 0x22))
    d.text((486, 372), "Instant file search and folder sizes", font=font(40, False), fill=(0x4A, 0x52, 0x4F))
    d.text((486, 424), "for Windows, with an MCP server for AI", font=font(40, False), fill=(0x4A, 0x52, 0x4F))
    d.rectangle((0, h - 14, w, h), fill=TEAL)
    img.save(path, optimize=True)


def main():
    assets = os.path.join(ROOT, "assets")
    images = os.path.join(ROOT, "docs", "images")
    os.makedirs(assets, exist_ok=True)
    write_ico(os.path.join(assets, "quickseek.ico"), [draw(s) for s in SIZES])
    for name, c in (("icon", TEAL), ("icon-dark", TEAL_DARK)):
        with open(os.path.join(images, name + ".svg"), "w", encoding="utf-8", newline="\n") as f:
            f.write(svg(c))
        draw_vector(256, c).save(os.path.join(images, name + ".png"), optimize=True)
    social_preview(os.path.join(images, "social-preview.png"))
    contact_sheet(os.path.join(images, "icon-sizes.png"))
    print("wrote assets/quickseek.ico, docs/images/icon*.svg/png, icon-sizes.png, social-preview.png")


if __name__ == "__main__":
    main()
