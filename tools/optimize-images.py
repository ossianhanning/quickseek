"""Shrinks the README screenshots in docs/images: UI captures have few
colours, so a 256-colour palette (no dithering) looks the same at a third
of the size. Skips a file if the palette version isn't smaller."""
import glob
import io
import os

from PIL import Image

root = os.path.join(os.path.dirname(__file__), "..", "docs", "images")
for f in sorted(glob.glob(os.path.join(root, "*.png"))):
    before = os.path.getsize(f)
    im = Image.open(f).convert("RGB")
    q = im.quantize(colors=256, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE)
    buf = io.BytesIO()
    q.save(buf, "PNG", optimize=True)
    if buf.tell() < before:
        with open(f, "wb") as out:
            out.write(buf.getvalue())
    print(f"{os.path.basename(f)}: {before // 1024} KB -> {os.path.getsize(f) // 1024} KB")
