#!/usr/bin/env python3
"""Draw the installer art from the project's decided mark, so installing looks like the box.

  app/desktop/installer/wizard-*.png   Windows (Inno Setup): the tall welcome/finish panel and the
                                       small corner mark, one file per display scale
  app/desktop/installer/dmg-*.png      macOS: the DMG window's background, 1x and 2x

The look is the floating box's (app/desktop/ui/style.css): a near-black panel, a white H with its
arcs and word lines knocked out, white text, the box's dim grey for the quiet line. Never hand-edit
the PNGs; re-run this instead (it needs Pillow, and Segoe UI or the Mac's system font).

Finder draws the DMG's icon names itself, black in light mode and white in dark mode, and cannot be
told otherwise. So the two icons sit on mid-grey tiles on which both stay readable (4.5:1 or
better either way), rather than straight on the black.
"""
import sys
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

sys.path.insert(0, str(Path(__file__).resolve().parent))
from make_icons import mark_polys, masks  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "app" / "desktop" / "installer"

PANEL = (23, 23, 23, 255)      # the box's rgba(0,0,0,.82) over a mid-grey desktop
INK = (255, 255, 255, 255)     # --ink
DIM = (167, 167, 167, 255)     # --dim, rgba(255,255,255,.62) over the panel, solid: Finder
                               # would put a see-through pixel over white
TILE = (118, 118, 118, 255)    # #767676: black and white names both pass 4.5:1 on it
NAME = "Huck's Voice to Text"
QUIET = "Powered by Project Playground"

# Inno Setup's modern wizard, at 100% and each larger display scale it picks from.
SCALES = [100, 125, 150, 175, 200, 225, 250]
WIZARD = (164, 314)
SMALL = (55, 55)
DMG = (660, 400)
# Finder icon centres in the DMG window, in points; release.sh places the icons here.
APP_AT, APPS_AT, ICON = (180, 190), (480, 190), 128


def font(size: float, bold: bool = False) -> ImageFont.FreeTypeFont:
    names = (["seguisb.ttf", "SFNS.ttf", "Helvetica.ttc"] if bold else ["segoeui.ttf", "SFNS.ttf", "Helvetica.ttc"])
    for name in names:
        for folder in [Path("C:/Windows/Fonts"), Path("/System/Library/Fonts")]:
            if (folder / name).exists():
                return ImageFont.truetype(str(folder / name), round(size))
    return ImageFont.load_default()


def white_mark(side: int) -> Image.Image:
    """The H in white, its arcs and word lines see-through, on nothing."""
    ink, _ = masks(side, mark_polys())
    out = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    out.paste(Image.new("RGBA", (side, side), INK), (0, 0), ink)
    return out


def centred(draw: ImageDraw.ImageDraw, x: float, y: float, text: str, f, fill) -> None:
    w = draw.textlength(text, font=f)
    draw.text((x - w / 2, y), text, font=f, fill=fill)


def wizard(scale: float) -> Image.Image:
    w, h = round(WIZARD[0] * scale), round(WIZARD[1] * scale)
    img = Image.new("RGBA", (w, h), PANEL)
    side = round(72 * scale)
    img.alpha_composite(white_mark(side), ((w - side) // 2, round(92 * scale)))
    d = ImageDraw.Draw(img)
    centred(d, w / 2, 178 * scale, "Huck's", font(15 * scale, True), INK)
    centred(d, w / 2, 197 * scale, "Voice to Text", font(15 * scale, True), INK)
    centred(d, w / 2, h - 30 * scale, QUIET, font(8.5 * scale), DIM)
    return img


def small(scale: float) -> Image.Image:
    w, h = round(SMALL[0] * scale), round(SMALL[1] * scale)
    img = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    side = round(40 * scale)
    img.alpha_composite(white_mark(side), ((w - side) // 2, (h - side) // 2))
    return img


def dmg(scale: int) -> Image.Image:
    s = scale
    img = Image.new("RGBA", (DMG[0] * s, DMG[1] * s), PANEL)
    d = ImageDraw.Draw(img)
    # The box's header: the H, the name, the quiet line.
    img.alpha_composite(white_mark(30 * s), (26 * s, 22 * s))
    d.text((66 * s, 22 * s), NAME, font=font(15 * s, True), fill=INK)
    d.text((66 * s, 41 * s), QUIET, font=font(10.5 * s), fill=DIM)
    # A tile under each icon and its name, like the box's raised surfaces.
    for cx, cy in (APP_AT, APPS_AT):
        box = [(cx - 88) * s, (cy - 84) * s, (cx + 88) * s, (cy + 104) * s]
        d.rounded_rectangle(box, radius=14 * s, fill=TILE)
    # The way across: a quiet line with a chevron, between the tiles.
    y = APP_AT[1] * s
    x0, x1 = (APP_AT[0] + 100) * s, (APPS_AT[0] - 100) * s
    d.line([(x0, y), (x1, y)], fill=DIM, width=3 * s)
    d.line([(x1 - 12 * s, y - 12 * s), (x1, y), (x1 - 12 * s, y + 12 * s)], fill=DIM, width=3 * s, joint="curve")
    centred(d, DMG[0] * s / 2, 336 * s, "Drag Huck's Voice to Text onto Applications", font(12.5 * s), DIM)
    return img


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for pct in SCALES:
        wizard(pct / 100).save(OUT / f"wizard-{pct}.png")
        small(pct / 100).save(OUT / f"wizard-small-{pct}.png")
    dmg(1).save(OUT / "dmg-background.png")
    dmg(2).save(OUT / "dmg-background@2x.png")
    print(f"Wrote installer art to {OUT}")


if __name__ == "__main__":
    main()
