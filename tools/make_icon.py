#!/usr/bin/env python3
"""Draw the pictures the PSP's menu shows for the app.

    uv run --with pillow tools/make_icon.py

ICON0.PNG (144x80) is the tile in the Game menu and PIC1.PNG (480x272) is the
backdrop behind it while the app is selected. Both use the character's
resting frame from the packed clip, so run tools/pack_video.py first.
"""
import pathlib

from PIL import Image, ImageDraw, ImageFont

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "assets" / "menu"
CELL_W, CELL_H = 168, 256
BACKGROUND = (247, 244, 248)
INK = (62, 48, 92)
MUTED = (138, 126, 160)


def character():
    cell = Image.open(ROOT / "assets" / "clip" / "00.png").crop((0, 0, CELL_W, CELL_H))
    return cell.crop(cell.getbbox())


def font(size):
    return ImageFont.truetype(str(next((ROOT / "assets" / "fonts").glob("*Bold*.ttf"))), size)


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    muse = character()

    icon = Image.new("RGB", (144, 80), BACKGROUND)
    small = muse.resize((round(muse.width * 72 / muse.height), 72), Image.Resampling.LANCZOS)
    icon.paste(small, (8, 6), small)
    ImageDraw.Draw(icon).text((8 + small.width + 8, 40), "Muse", font=font(26), fill=INK, anchor="lm")
    icon.save(OUT / "ICON0.PNG", optimize=True)

    # The menu draws its own text over the left, so the character stands on the right.
    backdrop = Image.new("RGB", (480, 272), BACKGROUND)
    large = muse.resize((round(muse.width * 230 / muse.height), 230), Image.Resampling.LANCZOS)
    backdrop.paste(large, (480 - large.width - 36, 272 - 230 - 14), large)
    draw = ImageDraw.Draw(backdrop)
    draw.text((36, 176), "Muse", font=font(44), fill=INK, anchor="ls")
    draw.text((38, 204), "Hold R and talk", font=font(16), fill=MUTED, anchor="ls")
    backdrop.save(OUT / "PIC1.PNG", optimize=True)
    print("wrote", OUT / "ICON0.PNG", "and", OUT / "PIC1.PNG")


if __name__ == "__main__":
    main()
