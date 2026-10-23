"""Makes the README's search-as-you-type animation from the frames that
tools/demo-shots.ps1 captured (target/demo-gui/frames/*.png): an animated
WebP in docs/images, scaled to 1x. With --dark, from frames-dark into
search-as-you-type-dark.webp."""
import glob
import os
import sys

from PIL import Image

root = os.path.join(os.path.dirname(__file__), "..")
dark = "--dark" in sys.argv
args = [a for a in sys.argv[1:] if a != "--dark"]
frames = sorted(glob.glob(os.path.join(root, "target", "demo-gui", "frames-dark" if dark else "frames", "*.png")))
if not frames:
    sys.exit("no frames: run tools\demo-shots.ps1 -Only search-typing first")
scale = float(args[0]) if args else 1.5
imgs = []
for f in frames:
    im = Image.open(f).convert("RGB")
    imgs.append(im.resize((round(im.width / scale), round(im.height / scale)), Image.LANCZOS))
# Short frames while typing, a pause on the empty box and on the result.
durations = [900] + [140] * (len(imgs) - 2) + [2600]
out = os.path.join(root, "docs", "images", "search-as-you-type" + ("-dark" if dark else ""))
imgs[0].save(out + ".webp", save_all=True, append_images=imgs[1:], duration=durations, loop=0, lossless=False, quality=90, method=6)


for ext in (".webp",):
    print(out + ext, os.path.getsize(out + ext) // 1024, "KB")
