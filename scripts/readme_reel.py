#!/usr/bin/env python3
"""Paint phimint ANSI frames into a README demo GIF.

Reads the `*.ans` frames written by `cargo test frame_reel -- --ignored`
(one file per frame, plain SGR colors as emitted by `snapshot_ansi`) and
renders them with Menlo into PNGs, then assembles a GIF with ffmpeg.

    python3 scripts/readme_reel.py                 # target/reel -> docs/assets/readme-demo.gif
    python3 scripts/readme_reel.py --frames DIR --out PATH

Pictographs (the plan-block symbols) come from Apple Color Emoji, which
only loads at its bitmap strike sizes, so they are drawn at 32px and
scaled into the two cells a wide glyph occupies.
"""

import argparse
import re
import shutil
import subprocess
import sys
import unicodedata
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

MENLO = "/System/Library/Fonts/Menlo.ttc"
EMOJI_FONT = "/System/Library/Fonts/Apple Color Emoji.ttc"

FONT_SIZE = 16
LINE_H = 22
PAD = 14
BG = (13, 17, 23)  # GitHub dark
DEFAULT_FG = (230, 237, 243)

# ANSI color slots on the dark canvas.
PALETTE = {
    30: (139, 148, 158), 31: (248, 81, 73), 32: (63, 185, 80),
    33: (210, 168, 60), 34: (88, 166, 255), 35: (210, 120, 255),
    36: (88, 196, 220), 37: (208, 215, 222),
    90: (139, 148, 158), 91: (255, 120, 110), 92: (110, 231, 130),
    93: (240, 200, 90), 94: (120, 190, 255), 95: (240, 160, 255),
    96: (120, 220, 240), 97: (255, 255, 255),
}

SGR_RE = re.compile(r"\x1b\[([0-9;]*)m")


def is_emoji(ch: str) -> bool:
    cp = ord(ch)
    return (
        0x1F000 <= cp <= 0x1FAFF
        or 0x2600 <= cp <= 0x27BF
        or 0x2B00 <= cp <= 0x2BFF
    )


class Style:
    def __init__(self):
        self.fg = DEFAULT_FG
        self.bg = BG
        self.bold = False
        self.dim = False
        self.italic = False
        self.underline = False
        self.reverse = False

    def apply(self, params: str) -> None:
        codes = [int(c) for c in params.split(";") if c != ""] or [0]
        i = 0
        while i < len(codes):
            c = codes[i]
            if c == 0:
                self.__init__()
            elif c == 1:
                self.bold = True
            elif c == 2:
                self.dim = True
            elif c == 3:
                self.italic = True
            elif c == 4:
                self.underline = True
            elif c == 7:
                self.reverse = True
            elif c == 22:
                self.bold = False
                self.dim = False
            elif c == 23:
                self.italic = False
            elif c == 24:
                self.underline = False
            elif c == 27:
                self.reverse = False
            elif 30 <= c <= 37 or 90 <= c <= 97:
                self.fg = PALETTE.get(c, DEFAULT_FG)
            elif c == 39:
                self.fg = DEFAULT_FG
            elif 40 <= c <= 47 or 100 <= c <= 107:
                self.bg = PALETTE.get(c if c < 100 else c - 60, BG)
            elif c == 49:
                self.bg = BG
            elif c == 38 and i + 2 < len(codes) and codes[i + 1] == 2:
                self.fg = tuple(codes[i + 2 : i + 5])
                i += 4
            elif c == 48 and i + 2 < len(codes) and codes[i + 1] == 2:
                self.bg = tuple(codes[i + 2 : i + 5])
                i += 4
            i += 1

    def colors(self):
        fg, bg = self.fg, self.bg
        if self.reverse:
            fg, bg = bg, fg
        if self.dim:
            fg = tuple((a + b) // 2 for a, b in zip(fg, bg))
        return fg, bg


def parse_frame(text: str):
    """Yield (row, col, style, char) laid out on the cell grid."""
    style = Style()
    rows = text.split("\n")
    for r, row in enumerate(rows):
        col = 0
        pos = 0
        while pos < len(row):
            m = SGR_RE.search(row, pos)
            if m and m.start() == pos:
                style.apply(m.group(1))
                pos = m.end()
                continue
            end = m.start() if m else len(row)
            for ch in row[pos:end]:
                yield r, col, style, ch
                col += 2 if unicodedata.east_asian_width(ch) in "WF" else 1
            pos = end
        # A trailing reset before the newline is harmless; style carries over
        # because `snapshot_ansi` resets at the end of every row anyway.


def render(frame_text: str, cols: int, rows: int, fonts, emoji_font):
    menlo = fonts
    adv = menlo["r"].getlength("0")
    w = int(adv * cols) + PAD * 2
    h = LINE_H * rows + PAD * 2
    img = Image.new("RGB", (w, h), BG)
    draw = ImageDraw.Draw(img)

    for r, col, style, ch in parse_frame(frame_text):
        if r >= rows or col >= cols:
            continue
        fg, bg = style.colors()
        x = PAD + col * adv
        y = PAD + r * LINE_H
        span = 2 if unicodedata.east_asian_width(ch) in "WF" else 1
        if bg != BG:
            draw.rectangle([x, y, x + adv * span, y + LINE_H], fill=bg)
        if ch == " ":
            continue
        font_key = (
            "bi"
            if (style.bold and style.italic)
            else ("b" if style.bold else ("i" if style.italic else "r"))
        )
        if is_emoji(ch):
            # Color glyphs: draw into a tile at the emoji font's strike size
            # (Apple Color Emoji is a bitmap font — only its strikes load),
            # then scale into the two cells a wide glyph occupies.
            tile = Image.new("RGBA", (48, 48), (0, 0, 0, 0))
            ImageDraw.Draw(tile).text(
                (24, 24), ch, font=emoji_font, embedded_color=True, anchor="mm"
            )
            cell_px = int(LINE_H * 0.95)
            tile = tile.resize((cell_px, cell_px), Image.LANCZOS)
            img.paste(tile, (int(x), int(y + (LINE_H - cell_px) / 2)), tile)
        else:
            draw.text(
                (x, y + LINE_H / 2),
                ch,
                font=menlo[font_key],
                fill=fg,
                anchor="lm",
            )
        if style.underline:
            draw.line(
                [x, y + LINE_H - 3, x + adv * span - 1, y + LINE_H - 3],
                fill=fg,
                width=1,
            )
    return img


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--frames", default="target/reel")
    ap.add_argument("--out", default="docs/assets/readme-demo.gif")
    ap.add_argument("--fps", type=float, default=2.2)
    ap.add_argument("--hold-last", type=int, default=6, help="extra final-frame copies")
    args = ap.parse_args()

    frames_dir = Path(args.frames)
    frames = sorted(frames_dir.glob("frame_*.ans"))
    if not frames:
        print(f"no frames in {frames_dir}; run: cargo test frame_reel -- --ignored", file=sys.stderr)
        return 1
    if not shutil.which("ffmpeg"):
        print("ffmpeg not found", file=sys.stderr)
        return 1

    fonts = {
        k: ImageFont.truetype(MENLO, FONT_SIZE, index=i)
        for k, i in (("r", 0), ("b", 1), ("i", 2), ("bi", 3))
    }
    emoji_font = ImageFont.truetype(EMOJI_FONT, 32)

    first = render(frames[0].read_text(), 100, 32, fonts, emoji_font)
    png_dir = Path(args.out).parent / "_reel_png"
    png_dir.mkdir(parents=True, exist_ok=True)

    outs = []
    for i, f in enumerate(frames):
        img = (
            first.copy()
            if i == 0
            else render(f.read_text(), 100, 32, fonts, emoji_font)
        )
        p = png_dir / f"p_{i:04}.png"
        img.save(p)
        outs.append(p)
    for _ in range(args.hold_last):
        p = png_dir / f"p_{len(outs):04}.png"
        shutil.copyfile(outs[-1], p)
        outs.append(p)

    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [
            "ffmpeg", "-y", "-loglevel", "error",
            "-framerate", str(args.fps),
            "-i", str(png_dir / "p_%04d.png"),
            "-vf", "split[s0][s1];[s0]palettegen=max_colors=192[p];[s1][p]paletteuse=dither=bayer:bayer_scale=3",
            "-loop", "0",
            str(out),
        ],
        check=True,
    )
    size = out.stat().st_size
    print(f"{out}  {len(frames)} frames + {args.hold_last} hold  {size/1024:.0f} KiB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
