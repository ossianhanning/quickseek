"""README images of an AI assistant using Quickseek's MCP server, from real
quickseek-mcp output on the demo PC (start `demo_service` first, see
tools/demo-shots.ps1). Renders a neutral chat-style page and the drawn
`present` charts with headless Edge at 1x and 2x into docs/images:

  mcp-chat.png / mcp-chat@1x.png        "where did my disk space go this month?"
  mcp-chart.png / mcp-chart@1x.png      the size-over-time chart from `present`

The assistant's text is written from the numbers in the tool results. No
vendor UI or logos: this is a generic illustration, not a screenshot.
"""
import html
import io
import json
import os
import subprocess
import tempfile
from datetime import datetime

from PIL import Image, ImageChops

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
MCP = os.path.join(ROOT, "target", "release", "quickseek-mcp.exe")
PIPE = "\\\\.\\pipe\\quickseek-demo"
EDGE = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
OUT = os.path.join(ROOT, "docs", "images")
BS = "\\"
GROW, SHRINK, INK, MUTED, GRID = "#d9534f", "#3b7dd8", "#1f2328", "#656d76", "#e6e8eb"
SERIES = ["#3b7dd8", "#e8833a", "#2a9d8f", "#9b5de5", "#c9a227", "#d9534f", "#6c757d"]


def mcp(*calls):
    lines = [json.dumps({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "readme", "version": "1"}}})]
    for i, (name, args) in enumerate(calls):
        lines.append(json.dumps({"jsonrpc": "2.0", "id": i + 1, "method": "tools/call", "params": {"name": name, "arguments": args}}))
    env = dict(os.environ, QUICKSEEK_PIPE=PIPE)
    out = subprocess.run([MCP], input="\n".join(lines) + "\n", capture_output=True, text=True, env=env, check=True).stdout
    res = {}
    for line in out.splitlines():
        m = json.loads(line)
        if m.get("id"):
            res[m["id"] - 1] = m["result"]["structuredContent"]
    return [res[i] for i in range(len(calls))]


def esc(s):
    return html.escape(str(s))


def diverging_svg(chart, width=640):
    data = chart["data"]
    row, label_w, pad = 30, 150, 64
    h = row * len(data) + 16
    mid = label_w + (width - label_w) / 2
    half = (width - label_w) / 2 - pad
    top = max(abs(d["value"]) for d in data) or 1
    out = [f'<svg viewBox="0 0 {width} {h}" width="{width}" height="{h}" role="img">']
    out.append(f'<line x1="{mid}" y1="0" x2="{mid}" y2="{h}" stroke="{GRID}" stroke-width="1"/>')
    for i, d in enumerate(data):
        y = 8 + i * row
        v = d["value"]
        w = abs(v) / top * half
        x = mid if v >= 0 else mid - w
        color = GROW if v >= 0 else SHRINK
        out.append(f'<text x="{label_w - 12}" y="{y + 15}" text-anchor="end" fill="{INK}" font-size="13">{esc(d["label"])}</text>')
        out.append(f'<rect x="{x:.1f}" y="{y + 3}" width="{max(w, 1.5):.1f}" height="{row - 10}" rx="3" fill="{color}"/>')
        tx, anchor = (x + w + 6, "start") if v >= 0 else (x - 6, "end")
        out.append(f'<text x="{tx:.1f}" y="{y + 15}" text-anchor="{anchor}" fill="{MUTED}" font-size="12">{"+" if v > 0 else ""}{v:g} {chart["unit"]}</text>')
    out.append("</svg>")
    return "".join(out)


def parse_time(s, now):
    return now if s == "now" else datetime.strptime(s, "%Y-%m-%d %H:%M")


def line_svg(chart, width=760, height=320, skip_first=True):
    now = datetime.now()
    xs = [parse_time(t, now) for t in chart["data"]["x"]]
    series = chart["data"]["series"][1:] if skip_first else chart["data"]["series"]
    left, right, top, bottom = 48, 150, 12, 30
    vals = [v for s in series for v in s["values"] if v is not None]
    vmax = max(vals) * 1.08
    t0, t1 = xs[0].timestamp(), xs[-1].timestamp()
    px = lambda t: left + (t.timestamp() - t0) / (t1 - t0) * (width - left - right)
    py = lambda v: top + (1 - v / vmax) * (height - top - bottom)
    out = [f'<svg viewBox="0 0 {width} {height}" width="{width}" height="{height}" role="img">']
    step = 10 if vmax < 60 else 20 if vmax < 120 else 50
    v = 0
    while v <= vmax:
        out.append(f'<line x1="{left}" x2="{width - right}" y1="{py(v):.1f}" y2="{py(v):.1f}" stroke="{GRID}"/>')
        out.append(f'<text x="{left - 8}" y="{py(v) + 4:.1f}" text-anchor="end" fill="{MUTED}" font-size="11">{v}</text>')
        v += step
    out.append(f'<text x="{left - 8}" y="{top - 2}" text-anchor="end" fill="{MUTED}" font-size="11">{chart["unit"]}</text>')
    for i, t in enumerate(xs):
        if i in (0, len(xs) - 1) or (t.day in (1, 15) and t.hour < 6):
            out.append(f'<text x="{px(t):.1f}" y="{height - 10}" text-anchor="middle" fill="{MUTED}" font-size="11">{"now" if i == len(xs) - 1 else t.strftime("%b %d")}</text>')
    ends = []
    for k, s in enumerate(series):
        color = SERIES[k % len(SERIES)]
        pts = [(px(t), py(v)) for t, v in zip(xs, s["values"]) if v is not None]
        d = " ".join(f"{'M' if j == 0 else 'L'}{x:.1f},{y:.1f}" for j, (x, y) in enumerate(pts))
        out.append(f'<path d="{d}" fill="none" stroke="{color}" stroke-width="2.2" stroke-linejoin="round"/>')
        ends.append([pts[-1][1], s["label"], s["values"][-1], color])
    ends.sort()
    for j in range(1, len(ends)):  # keep the end labels apart
        ends[j][0] = max(ends[j][0], ends[j - 1][0] + 15)
    for y, label, v, color in ends:
        out.append(f'<text x="{width - right + 8}" y="{y + 4:.1f}" fill="{color}" font-size="12" font-weight="600">{esc(label)} <tspan fill="{MUTED}" font-weight="400">{v:g}</tspan></text>')
    out.append("</svg>")
    return "".join(out)


CSS = f"""
* {{ box-sizing: border-box; }}
body {{ margin: 0; background: #f6f7f9; font-family: "Segoe UI", system-ui, sans-serif; color: {INK}; }}
.page {{ width: 820px; padding: 28px; }}
.bar {{ font-size: 13px; color: {MUTED}; margin-bottom: 18px; }}
.user {{ display: flex; justify-content: flex-end; margin-bottom: 18px; }}
.user div {{ background: #e3e8ef; border-radius: 16px; padding: 10px 16px; font-size: 15px; max-width: 70%; }}
.ai {{ background: white; border: 1px solid {GRID}; border-radius: 14px; padding: 18px 20px; }}
.tool {{ display: inline-block; font: 12px Consolas, monospace; color: {MUTED}; background: #f1f3f5; border-radius: 6px; padding: 4px 8px; margin: 0 6px 8px 0; }}
.tool b {{ color: {INK}; font-weight: 600; }}
.card {{ border: 1px solid {GRID}; border-radius: 10px; padding: 14px 16px 6px; margin: 10px 0 14px; }}
.card h3 {{ margin: 0 0 2px; font-size: 14px; font-weight: 600; }}
.card p {{ margin: 0 0 8px; font-size: 12px; color: {MUTED}; }}
.ai p.say {{ font-size: 14.5px; line-height: 1.55; margin: 8px 0; }}
.legend {{ font-size: 12px; color: {MUTED}; margin: 4px 0 6px; }}
.legend span {{ display: inline-block; width: 10px; height: 10px; border-radius: 2px; margin: 0 4px -1px 10px; }}
"""


def page(body):
    return f'<!doctype html><html><head><meta charset="utf-8"><style>{CSS}</style></head><body><div class="page">{body}</div></body></html>'


def render(html_text, name, width=876):
    with tempfile.TemporaryDirectory() as d:
        src = os.path.join(d, "page.html")
        with open(src, "w", encoding="utf-8") as f:
            f.write(html_text)
        for scale, suffix in ((2, ""), (1, "@1x")):
            shot = os.path.join(d, f"shot{scale}.png")
            subprocess.run([EDGE, "--headless=new", "--disable-gpu", "--hide-scrollbars", f"--force-device-scale-factor={scale}",
                            f"--window-size={width},2400", f"--screenshot={shot}", "file:///" + src.replace(BS, "/")],
                           check=True, capture_output=True)
            im = Image.open(shot).convert("RGB")
            bg = Image.new("RGB", im.size, im.getpixel((im.width - 1, im.height - 1)))
            box = ImageChops.difference(im, bg).getbbox()
            m = 28 * scale
            im = im.crop((0, 0, min(im.width, box[2] + m), min(im.height, box[3] + m)))
            buf = io.BytesIO()
            im.quantize(colors=256, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE).save(buf, "PNG", optimize=True)
            path = os.path.join(OUT, f"{name}{suffix}.png")
            with open(path, "wb") as f:
                f.write(buf.getvalue())
            print(f"{path}: {im.width} x {im.height}, {len(buf.getvalue()) // 1024} KB")


def gb(v):
    return f"{v:.1f} GB" if abs(v) < 1000 else f"{v / 1024:.2f} TB"


def main():
    overview, growth = mcp(
        ("disk_overview", {}),
        ("folder_growth", {"path": "C:" + BS + "Users" + BS + "alex", "days": 30, "depth": 3, "limit": 6}),
    )
    line, div = growth["present"]
    c = next(v for v in overview["volumes"] if v["volume"] == "C")
    kids = {k["name"]: k for k in growth["children"] if "path" in k}

    def kid(path):
        node = {"children": growth["children"]}
        for part in path.split("/"):
            node = next(k for k in node["children"] if k.get("name") == part)
        return node

    since = datetime.strptime(growth["compared_with"]["time"], "%Y-%m-%d %H:%M").strftime("%B %d").replace(" 0", " ")
    dev, vids, pics, dl = kids["dev"], kids["Videos"], kids["Pictures"], kids["Downloads"]
    ml, te, cap = kid("dev/ml-experiments"), kid("dev/tilemap-editor"), kid("Videos/Captures")
    say = (
        f"<p class=say>Your user folder grew by <b>{esc(growth['change_human'].lstrip('+'))}</b> since {since} "
        f"({esc(growth['then_human'])} → {esc(growth['now_human'])}), and C: is down to "
        f"<b>{esc(c['free_human'])} free</b> ({c['used_percent']}% used). Where it went:</p>"
        f"<p class=say>• <b>dev {esc(dev['change_human'])}</b>: model checkpoints in ml-experiments ({esc(ml['change_human'])}) "
        f"and the Rust <code>target</code> folder of tilemap-editor ({esc(te['change_human'])}).<br>"
        f"• <b>Game captures {esc(cap['change_human'])}</b> in Videos\\Captures.<br>"
        f"• <b>Phone photos {esc(pics['change_human'])}</b> in Pictures\\Camera Roll.<br>"
        f"• Downloads went the other way, <b>{esc(dl['change_human'])}</b>, from the cleanup a week ago.</p>"
        f"<p class=say>The quickest wins: old checkpoints and the <code>target</code> folder (rebuilt by <code>cargo build</code>). "
        f"Want me to list what <code>cleanup_candidates</code> finds?</p>"
    )
    chat = (
        '<div class="bar">AI assistant · Quickseek MCP server</div>'
        '<div class="user"><div>Where did my disk space go this month?</div></div>'
        '<div class="ai">'
        '<span class="tool"><b>disk_overview</b>()</span>'
        f'<span class="tool"><b>folder_growth</b>(path: "C:\\Users\\alex", days: 30, depth: 3)</span>'
        f'<div class="card"><h3>{esc(div["title"])}</h3><p>{esc(div["why"])}</p>{diverging_svg(div)}</div>'
        f"{say}</div>"
    )
    render(page(chat), "mcp-chat")
    legend = f'<div class="legend">Each line is a folder in C:\\Users\\alex (which went {esc(growth["then_human"])} → {esc(growth["now_human"])}).</div>'
    chart = f'<div class="card" style="background:white"><h3>{esc(line["title"])}</h3>{legend}{line_svg(line)}</div>'
    render(page(chart), "mcp-chart")


if __name__ == "__main__":
    main()
