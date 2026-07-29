#!/usr/bin/env python3
"""Render the TUI screenshot grids into the PNGs the README embeds.

Run after the generator has produced the grids:

    cargo test --lib readme_screenshots -- --ignored --nocapture
    python3 scripts/render_screenshots.py

The two halves are split because nothing that renders SVG here draws colour
emoji — librsvg turns them into black outlines, which on a dark terminal
background is worse than leaving them out, and the padlocks are half the point
of these pictures. Pillow renders the bitmap emoji strikes properly, so the Rust
side dumps a grid of cells and this side paints it.

Needs Pillow, a monospace font and Noto Color Emoji; it says which is missing
rather than producing something subtly wrong.
"""

import json
import pathlib
import sys

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError:
    sys.exit("Pillow is required: pip install --user Pillow")

ROOT = pathlib.Path(__file__).resolve().parent.parent
GRIDS = ROOT / "target" / "screenshots"
OUT = ROOT / "docs" / "screenshots"

# Rendered at 2x a comfortable terminal size, so the PNGs stay sharp when GitHub
# scales them down.
FONT_SIZE = 28
BACKGROUND = (14, 17, 22, 255)

MONO_CANDIDATES = [
    "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
    "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
]
BOLD_CANDIDATES = [
    "/usr/share/fonts/TTF/JetBrainsMono-Bold.ttf",
    "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Bold.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf",
]
EMOJI_CANDIDATES = [
    "/usr/share/fonts/noto/NotoColorEmoji.ttf",
    "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
]


def first_existing(paths, what):
    for p in paths:
        if pathlib.Path(p).exists():
            return p
    sys.exit(f"no {what} found; looked in:\n  " + "\n  ".join(paths))


def load_emoji_font(path):
    """Colour emoji are bitmap strikes at one fixed size, so ask the font which."""
    for size in (109, 128, 96, 64, 32):
        try:
            return ImageFont.truetype(path, size), size
        except OSError:
            continue
    sys.exit(f"could not load {path} at any known strike size")


def rgb(value, fallback):
    if not value:
        return fallback
    v = value.lstrip("#")
    return (int(v[0:2], 16), int(v[2:4], 16), int(v[4:6], 16), 255)


def is_emoji(symbol):
    return any(ord(c) >= 0x1F000 for c in symbol)


def render(grid_path, out_path, mono, bold, emoji_font, emoji_size, cw, ch, ascent):
    grid = json.loads(grid_path.read_text())
    width, height = grid["w"] * cw, grid["h"] * ch
    img = Image.new("RGBA", (width, height), BACKGROUND)
    draw = ImageDraw.Draw(img)

    # Backgrounds first, so a selected row's highlight sits under its text.
    for cell in grid["cells"]:
        if "bg" in cell:
            x, y = cell["x"] * cw, cell["y"] * ch
            draw.rectangle([x, y, x + cw, y + ch], fill=rgb(cell["bg"], BACKGROUND))

    emoji_cache = {}
    for cell in grid["cells"]:
        symbol = cell["s"]
        if not symbol.strip():
            continue
        x, y = cell["x"] * cw, cell["y"] * ch

        if is_emoji(symbol):
            # One bitmap strike, scaled to the cell — an emoji occupies two
            # columns in a terminal, which is what ratatui reserved for it.
            if symbol not in emoji_cache:
                tile = Image.new("RGBA", (emoji_size, emoji_size), (0, 0, 0, 0))
                ImageDraw.Draw(tile).text(
                    (0, 0), symbol, font=emoji_font, embedded_color=True
                )
                emoji_cache[symbol] = tile.resize((cw * 2, cw * 2), Image.LANCZOS)
            tile = emoji_cache[symbol]
            img.alpha_composite(tile, (int(x), int(y + (ch - cw * 2) // 2)))
            continue

        font = bold if cell.get("b") else mono
        draw.text((x, y + ascent), symbol, font=font, fill=rgb(cell.get("fg"), (205, 217, 229, 255)),
                  anchor="ls")

    OUT.mkdir(parents=True, exist_ok=True)
    img.convert("RGB").save(out_path)
    print(f"wrote {out_path.relative_to(ROOT)}  ({width}x{height})")


def main():
    if not GRIDS.is_dir():
        sys.exit(
            "no grids found — run first:\n"
            "    cargo test --lib readme_screenshots -- --ignored --nocapture"
        )

    mono_path = first_existing(MONO_CANDIDATES, "monospace font")
    bold_path = first_existing(BOLD_CANDIDATES, "bold monospace font")
    emoji_path = first_existing(EMOJI_CANDIDATES, "colour emoji font")

    mono = ImageFont.truetype(mono_path, FONT_SIZE)
    bold = ImageFont.truetype(bold_path, FONT_SIZE)
    emoji_font, emoji_size = load_emoji_font(emoji_path)

    # Cell size from the font itself: box-drawing glyphs span exactly one line,
    # so any extra leading leaves gaps in every panel border.
    cw = round(mono.getlength("M"))
    ascent, descent = mono.getmetrics()
    ch = ascent + descent

    for grid_path in sorted(GRIDS.glob("*.json")):
        render(
            grid_path,
            OUT / f"{grid_path.stem}.png",
            mono, bold, emoji_font, emoji_size, cw, ch, ascent,
        )


if __name__ == "__main__":
    main()
