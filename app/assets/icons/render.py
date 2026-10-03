"""Renders the app icon: the constellation from icons::constellation(), on
the dark tile. Writes the desktop PNGs, icon.ico, icon.svg and the Android
adaptive icon layers.

    python3 assets/icons/render.py    # from app/, needs Pillow
"""

from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).parent
RES = HERE.parent.parent / "android" / "res"

TILE = (18, 20, 28)
LINE = (122, 162, 247, 150)
STAR = (224, 122, 95)
SPARK = (229, 231, 239)

# The same drawing as icons::constellation(), in its 24-unit box.
PATH = [(3, 20.5), (7, 16.5), (11, 15), (13, 10), (19.5, 11.5), (21, 5), (15, 3.5), (13, 10)]
STARS = [(7, 16.5, 1.4), (11, 15, 1.4), (13, 10, 1.8), (19.5, 11.5, 1.8), (15, 3.5, 1.5)]
SPARKS = [(21, 5.7, 3.9), (3, 20.7, 2.7)]
STROKE = 1.3
# Fainter stars around it, the rest of the map.
DIM = [(5.5, 6), (9, 3), (17.2, 7.6), (22.5, 17.5), (17, 20.5), (10.5, 21.5)]
DIM_R, DIM_ALPHA = 0.45, 90

# Centre of the drawing, and how many pixels a unit is on a 1024 tile. Small
# enough that the corner sparkles stay inside an adaptive icon's safe zone.
CX, CY = 12.6, 12.6
UNIT = 30
RADIUS = 224


def spark(x, y, r):
    """A four-pointed star, the shape of the sparkles in the SVG."""
    k = r * 0.26
    return [(x, y - r), (x + k, y - k), (x + r, y), (x + k, y + k),
            (x, y + r), (x - k, y + k), (x - r, y), (x - k, y - k)]


def draw(size, tile=True, ink=None, scale=1.0):
    """The icon at `size`, drawn 4x and scaled down for the antialiasing."""
    s = size * 4
    unit = UNIT * s / 1024 * scale
    at = lambda x, y: (s / 2 + (x - CX) * unit, s / 2 + (y - CY) * unit)

    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    if tile:
        ImageDraw.Draw(img).rounded_rectangle((0, 0, s - 1, s - 1), RADIUS * s / 1024, fill=TILE)

    # The line on a layer of its own, so its overlaps do not double up.
    line = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    ld = ImageDraw.Draw(line)
    width = round(STROKE * unit)
    points = [at(x, y) for x, y in PATH]
    ld.line(points, fill=(ink or LINE)[:3] + (255,), width=width, joint="curve")
    for p in (points[0], points[-1]):
        ld.ellipse((p[0] - width / 2, p[1] - width / 2, p[0] + width / 2, p[1] + width / 2), fill=(ink or LINE)[:3] + (255,))
    line.putalpha(line.getchannel("A").point(lambda a: a * LINE[3] // 255))
    img.alpha_composite(line)

    dim = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    dd = ImageDraw.Draw(dim)
    for x, y in DIM:
        (px, py), pr = at(x, y), DIM_R * unit
        dd.ellipse((px - pr, py - pr, px + pr, py + pr), fill=(ink or SPARK)[:3] + (DIM_ALPHA,))
    img.alpha_composite(dim)

    d = ImageDraw.Draw(img)
    for x, y, r in STARS:
        (px, py), pr = at(x, y), r * unit
        d.ellipse((px - pr, py - pr, px + pr, py + pr), fill=ink or STAR)
    for x, y, r in SPARKS:
        d.polygon([at(px, py) for px, py in spark(x, y, r)], fill=ink or SPARK)

    return img.resize((size, size), Image.LANCZOS)


def svg():
    k = 1024 / 24
    t = lambda v: f"{v:g}"
    pts = " ".join(f"{t(x)},{t(y)}" for x, y in PATH)
    stars = "\n".join(f'    <circle cx="{t(x)}" cy="{t(y)}" r="{t(r)}"/>' for x, y, r in STARS)
    sparks = "\n".join(
        '    <path d="M' + " ".join(f"{t(round(px, 3))},{t(round(py, 3))}" for px, py in spark(x, y, r)) + 'z"/>'
        for x, y, r in SPARKS
    )
    dim = "\n".join(f'    <circle cx="{t(x)}" cy="{t(y)}" r="{t(DIM_R)}"/>' for x, y in DIM)
    scale = UNIT / k
    rgb = lambda c: "#%02x%02x%02x" % c[:3]
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">
  <rect width="24" height="24" rx="{t(round(RADIUS / k, 3))}" fill="{rgb(TILE)}"/>
  <g transform="translate(12 12) scale({t(round(scale, 4))}) translate({t(-CX)} {t(-CY)})">
    <g fill="{rgb(SPARK)}" fill-opacity="{t(round(DIM_ALPHA / 255, 2))}">
{dim}
    </g>
    <polyline points="{pts}" fill="none" stroke="{rgb(LINE)}" stroke-opacity="{t(round(LINE[3] / 255, 2))}"
      stroke-width="{t(STROKE)}" stroke-linecap="round" stroke-linejoin="round"/>
    <g fill="{rgb(STAR)}">
{stars}
    </g>
    <g fill="{rgb(SPARK)}">
{sparks}
    </g>
  </g>
</svg>
"""


def main():
    for size in (32, 64, 128, 256, 512, 1024):
        draw(size).save(HERE / f"{size}x{size}.png", optimize=True)
    draw(256).save(HERE / "icon.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    (HERE / "icon.svg").write_text(svg())

    # Adaptive icon: 108dp layers, of which a launcher shows the middle 72dp.
    for name, dp in [("mdpi", 1), ("hdpi", 1.5), ("xhdpi", 2), ("xxhdpi", 3), ("xxxhdpi", 4)]:
        size = round(108 * dp)
        out = RES / f"mipmap-{name}"
        out.mkdir(parents=True, exist_ok=True)
        draw(size, tile=False, scale=72 / 108).save(out / "ic_launcher_foreground.png", optimize=True)
        draw(size, tile=False, scale=72 / 108, ink=(255, 255, 255)).save(out / "ic_launcher_monochrome.png", optimize=True)


if __name__ == "__main__":
    main()
