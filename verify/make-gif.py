"""Assemble docs/demo.gif from the frames verify/demo-extension.js recorded.

Crops to the area that ever changes (plus the top bar), draws the mouse pointer and click ripples
from the logged cursor path (area screenshots don't include the pointer), scales down, and encodes a
palette-optimised GIF with ffmpeg. Usage: python3 -I make-gif.py <recording-dir> <out.gif>
"""
import pathlib
import subprocess
import sys
import tempfile

from PIL import Image, ImageChops, ImageDraw

SCALE = 0.78          # README width ~ 420 px
HOLD_LAST = 1.0       # seconds on the last frame before the loop restarts
MARGIN = 18

rec, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
frames = [(rec / "frames" / n, float(t) / 1000) for n, t in
          (l.split() for l in (rec / "frames.txt").read_text().splitlines() if l.strip())]
moves = []
for l in (rec / "cursor.txt").read_text().splitlines():
    p = l.split()
    moves.append((float(p[0]) / 1000, float(p[1]), float(p[2]), len(p) > 3))
clicks = [m[0] for m in moves if m[3]]

# Crop: everything that differs from the plain background in any frame, always from the top edge.
first = Image.open(frames[0][0]).convert("RGB")
bg = Image.new("RGB", first.size, first.getpixel((2, first.height - 3)))
box = None
for path, _ in frames[::3] + frames[-1:]:
    b = ImageChops.difference(Image.open(path).convert("RGB"), bg).getbbox()
    if b:
        box = b if box is None else (min(box[0], b[0]), min(box[1], b[1]), max(box[2], b[2]), max(box[3], b[3]))
x0, y0 = max(0, box[0] - MARGIN), 0
x1, y1 = min(first.width, box[2] + MARGIN), min(first.height, box[3] + MARGIN)

ARROW = [(0, 0), (0, 19), (4.6, 14.6), (8.2, 22), (11.2, 20.6), (7.8, 13.6), (14, 13.6)]


def cursor_at(t):
    pos = moves[0][1:3]
    for mt, x, y, _ in moves:
        if mt > t:
            break
        pos = (x, y)
    return pos


def draw_pointer(img, t):
    over = Image.new("RGBA", img.size, (0, 0, 0, 0))
    d = ImageDraw.Draw(over)
    cx, cy = cursor_at(t)
    cx, cy = cx - x0, cy - y0
    for tc in clicks:
        p = (t - tc) / 0.38
        if 0 <= p <= 1:
            r = 9 + 20 * p
            d.ellipse((cx - r, cy - r, cx + r, cy + r), outline=(255, 255, 255, int(170 * (1 - p))), width=3)
    shadow = [(cx + x + 1.5, cy + y + 2) for x, y in ARROW]
    d.polygon(shadow, fill=(0, 0, 0, 90))
    pts = [(cx + x, cy + y) for x, y in ARROW]
    d.polygon(pts, fill=(255, 255, 255, 255), outline=(20, 20, 24, 255), width=2)
    return Image.alpha_composite(img.convert("RGBA"), over).convert("RGB")


with tempfile.TemporaryDirectory() as tmp:
    tmp = pathlib.Path(tmp)
    size = (round((x1 - x0) * SCALE) // 2 * 2, round((y1 - y0) * SCALE) // 2 * 2)
    lines = ["ffconcat version 1.0"]
    for i, (path, t) in enumerate(frames):
        img = Image.open(path).convert("RGB").crop((x0, y0, x1, y1))
        img = draw_pointer(img, t).resize(size, Image.LANCZOS)
        name = tmp / f"{i:04d}.png"
        img.save(name)
        dur = frames[i + 1][1] - t if i + 1 < len(frames) else HOLD_LAST
        lines += [f"file '{name}'", f"duration {max(dur, 0.02):.3f}"]
    lines.append(f"file '{tmp / f'{len(frames) - 1:04d}.png'}'")
    (tmp / "list.txt").write_text("\n".join(lines) + "\n")
    common = ["ffmpeg", "-v", "error", "-y", "-f", "concat", "-safe", "0", "-i", str(tmp / "list.txt")]
    subprocess.run(common + ["-vf", "palettegen=max_colors=256:stats_mode=full", str(tmp / "pal.png")], check=True)
    subprocess.run(common + ["-i", str(tmp / "pal.png"), "-lavfi",
                             "paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle", "-loop", "0", str(out)],
                   check=True)
print(f"{out}: {len(frames)} frames, {size[0]}x{size[1]}, {frames[-1][1]:.1f} s, "
      f"{out.stat().st_size / 1e6:.2f} MB")
