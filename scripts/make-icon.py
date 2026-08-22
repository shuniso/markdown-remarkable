#!/usr/bin/env python3
"""Regenerates packaging/macos/mdview.icns (a flat "M" tile).

Requires Pillow and macOS's `iconutil`. The .icns is committed, so this only
needs re-running when the artwork changes.
"""
import pathlib
import shutil
import subprocess
import sys
import tempfile

from PIL import Image, ImageDraw, ImageFont

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "packaging" / "macos" / "mdview.icns"

BG = (36, 41, 46)        # GitHub-dark-ish
FG = (88, 166, 255)      # accent blue
ACCENT = (255, 255, 255)


def draw(size: int) -> Image.Image:
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    pad = size * 0.06
    radius = size * 0.22
    d.rounded_rectangle((pad, pad, size - pad, size - pad), radius=radius, fill=BG)
    # "M" glyph, rendered with the system font if available.
    text = "M"
    font = None
    for candidate in (
        "/System/Library/Fonts/SFCompact.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/Library/Fonts/Arial Bold.ttf",
    ):
        try:
            font = ImageFont.truetype(candidate, int(size * 0.62))
            break
        except OSError:
            continue
    if font is None:
        font = ImageFont.load_default()
    bbox = d.textbbox((0, 0), text, font=font)
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    x = (size - tw) / 2 - bbox[0]
    y = (size - th) / 2 - bbox[1] - size * 0.04
    d.text((x, y), text, font=font, fill=ACCENT)
    # Down-arrow bar under the M, hinting "view / render".
    bar_h = size * 0.05
    d.rounded_rectangle(
        (size * 0.28, size * 0.80, size * 0.72, size * 0.80 + bar_h),
        radius=bar_h / 2,
        fill=FG,
    )
    return img


def main() -> int:
    if shutil.which("iconutil") is None:
        print("iconutil not found (macOS only)", file=sys.stderr)
        return 1
    with tempfile.TemporaryDirectory() as tmp:
        iconset = pathlib.Path(tmp) / "mdview.iconset"
        iconset.mkdir()
        for base in (16, 32, 128, 256, 512):
            draw(base).save(iconset / f"icon_{base}x{base}.png")
            draw(base * 2).save(iconset / f"icon_{base}x{base}@2x.png")
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(OUT)], check=True)
    print(f"wrote {OUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
