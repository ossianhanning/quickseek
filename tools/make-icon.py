"""Draws Quickseek's icon (concept B: a lens with speed lines) and writes:

  assets/quickseek.ico           16-256 px, each size drawn for itself
  docs/images/icon.svg           the full drawing, teal for light backgrounds
  docs/images/icon-dark.svg      lighter teal, for dark backgrounds
  docs/images/icon.png, icon-dark.png          256 px
  docs/images/social-preview.png               1280x640 with the name (GitHub)

Small sizes can't show the speed lines: 16-24 px are the lens and handle
alone, 32-48 px add one thick line, 64 px and up have all three. Shapes are
drawn 8x larger and scaled down, with the lens ring and handle widths
rounded to whole pixels at each size so they stay crisp.

    python tools/make-icon.py
"""
import io
import os
import struct

from PIL import Image, ImageDraw, ImageFont

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
TEAL = (0x0F, 0x6E, 0x56)
TEAL_DARK = (0x5D, 0xCA, 0xA5)  # on dark backgrounds
SIZES = [16, 20, 24, 32, 40, 48, 64, 256]

# The drawing in a 100x100 box (the reference geometry, centred).
LENS = (58, 44, 25)  # cx, cy, r (ring centre line)
RING = 12
HANDLE = ((77, 63), (93, 79), 14)
LINES = [  # x1, x2, y, opacity
    (6, 22, 32, 0.55),
    (2, 24, 46, 0.75),
    (10, 26, 60, 0.55),
]


def variant(size):
    if size <= 24:
        return "lens"
    if size <= 48:
        return "one-line"
    return "full"


def draw(size, color, kind=None):
    """The icon at `size` px, transparent background."""
    kind = kind or variant(size)
    ss = 8
    n = size * ss
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    u = n / 100.0  # one unit of the 100-box in supersampled px
    px = n / size  # one output pixel
    if kind == "lens":
        # The lens and handle fill the box: shift right and scale up.
        cx, cy, r = 41, 41, 31
        ring = max(2, round(14 / 100 * size)) * px
        h0, h1, hw = (66, 66), (88, 88), max(3, round(19 / 100 * size)) * px
        lines = []
    else:
        cx, cy, r = LENS
        ring = max(2, round(RING / 100 * size)) * px
        h0, h1, hw = HANDLE[0], HANDLE[1], max(3, round(HANDLE[2] / 100 * size)) * px
        if kind == "one-line":
            lines = [(0, 26, 46, 1.0, max(2, round(9 / 100 * size)) * px)]
        else:
            lines = [(a, b, y, o, max(2, round(7 / 100 * size)) * px) for a, b, y, o in LINES]
    cx, cy, r = cx * u, cy * u, r * u
    rgb = tuple(color)
    # Ring: an outer disc minus an inner one.
    ring_layer = Image.new("L", (n, n), 0)
    rd = ImageDraw.Draw(ring_layer)
    rd.ellipse((cx - r - ring / 2, cy - r - ring / 2, cx + r + ring / 2, cy + r + ring / 2), fill=255)
    rd.ellipse((cx - r + ring / 2, cy - r + ring / 2, cx + r - ring / 2, cy + r - ring / 2), fill=0)
    # Handle: a thick line with round caps.
    (x0, y0), (x1, y1) = (h0[0] * u, h0[1] * u), (h1[0] * u, h1[1] * u)
    rd.line((x0, y0, x1, y1), fill=255, width=int(hw))
    for (x, y) in ((x0, y0), (x1, y1)):
        rd.ellipse((x - hw / 2, y - hw / 2, x + hw / 2, y + hw / 2), fill=255)
    img.paste(Image.new("RGBA", (n, n), rgb + (255,)), (0, 0), ring_layer)
    for a, b, y, o, w in lines:
        layer = Image.new("L", (n, n), 0)
        ld = ImageDraw.Draw(layer)
        xa, xb, yy = a * u + w / 2, b * u - w / 2, y * u
        ld.rectangle((xa, yy - w / 2, xb, yy + w / 2), fill=255)
        for x in (xa, xb):
            ld.ellipse((x - w / 2, yy - w / 2, x + w / 2, yy + w / 2), fill=255)
        layer = layer.point(lambda v: int(v * o))
        tinted = Image.new("RGBA", (n, n), rgb + (255,))
        img = Image.alpha_composite(img, Image.composite(tinted, Image.new("RGBA", (n, n), (0, 0, 0, 0)), layer))
    return img.resize((size, size), Image.LANCZOS)


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
    icon = draw(256, TEAL, "full").resize((300, 300), Image.LANCZOS)
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
    write_ico(os.path.join(assets, "quickseek.ico"), [draw(s, TEAL) for s in SIZES])
    for name, c in (("icon", TEAL), ("icon-dark", TEAL_DARK)):
        with open(os.path.join(images, name + ".svg"), "w", encoding="utf-8", newline="\n") as f:
            f.write(svg(c))
        draw(256, c).save(os.path.join(images, name + ".png"), optimize=True)
    social_preview(os.path.join(images, "social-preview.png"))
    # A contact sheet of every size, for checking (not committed).
    sheet = Image.new("RGBA", (sum(SIZES) + 20 * len(SIZES) + 20, 2 * 276 + 20), (255, 255, 255, 255))
    x = 20
    for s in SIZES:
        sheet.alpha_composite(draw(s, TEAL), (x, 20))
        dark = Image.new("RGBA", (s + 8, s + 8), (0x20, 0x20, 0x20, 255))
        dark.alpha_composite(draw(s, TEAL_DARK), (4, 4))
        sheet.alpha_composite(dark, (x - 4, 296))
        x += s + 20
    out = os.path.join(ROOT, "target", "icon-sheet.png")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    sheet.save(out)
    print("wrote assets/quickseek.ico, docs/images/icon*.svg/png, social-preview.png;", out)


if __name__ == "__main__":
    main()
