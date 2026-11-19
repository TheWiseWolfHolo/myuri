"""Draws the Myuri icon: two wolf ears rising over the lower edge, which also
read as an M. Run from the repository root.

    python tools/make-icon.py            writes crates/myuri/assets/*.ico
    python tools/make-icon.py --preview  also writes target/icon-preview.png
"""
import math
import sys
from PIL import Image, ImageChops, ImageDraw, ImageFilter

S = 1024


def squircle(n=4.6, inset=0.035):
    r = S / 2 * (1 - inset * 2)
    points = []
    for i in range(720):
        t = 2 * math.pi * i / 720
        c, s = math.cos(t), math.sin(t)
        points.append((S / 2 + r * math.copysign(abs(c) ** (2 / n), c),
                       S / 2 + r * math.copysign(abs(s) ** (2 / n), s)))
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).polygon(points, fill=255)
    return mask


def curve(a, control, b, steps=40):
    return [((1 - t) ** 2 * a[0] + 2 * (1 - t) * t * control[0] + t * t * b[0],
             (1 - t) ** 2 * a[1] + 2 * (1 - t) * t * control[1] + t * t * b[1])
            for t in (i / steps for i in range(steps + 1))]


def ears(scale=1.0):
    """The silhouette as a mask. `scale` enlarges it about the bottom centre."""
    tip, valley, base = (0.238, 0.230), (0.500, 0.600), (0.128, 0.900)
    left = curve(base, (0.150, 0.520), tip) + curve(tip, (0.385, 0.372), valley)
    outline = left + [(1 - x, y) for x, y in reversed(left)] + [(0.872, 1.1), (0.128, 1.1)]
    outline = [(0.5 + (x - 0.5) * scale, 1 - (1 - y) * scale) for x, y in outline]
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).polygon([(x * S, y * S) for x, y in outline], fill=255)
    # Blurring and cutting back at half rounds every corner, tips and valley alike.
    mask = mask.filter(ImageFilter.GaussianBlur(S * 0.03)).point(lambda v: 255 if v >= 128 else 0)
    return mask.filter(ImageFilter.GaussianBlur(1.2))


def gradient(top, bottom, angle):
    ramp = Image.linear_gradient("L").resize((S * 2, S * 2)).rotate(angle, resample=Image.BICUBIC)
    ramp = ramp.crop((S // 2, S // 2, S // 2 + S, S // 2 + S))
    return Image.composite(Image.new("RGB", (S, S), bottom), Image.new("RGB", (S, S), top), ramp)


def draw(colors, small=False):
    tile_top, tile_bottom, fur_top, fur_bottom = colors
    art = gradient(tile_top, tile_bottom, 20)
    mark = ears(1.07 if small else 1.0)
    shade = ImageChops.offset(mark.filter(ImageFilter.GaussianBlur(S * 0.035)), 0, int(S * 0.014))
    art = Image.composite(Image.new("RGB", (S, S), tuple(int(c * 0.62) for c in tile_bottom)), art,
                          shade.point(lambda v: v * 0.38))
    art = Image.composite(gradient(fur_top, fur_bottom, 0), art, mark).convert("RGBA")
    art.putalpha(squircle())
    return art


def frames(colors):
    full, small = draw(colors), draw(colors, small=True)
    return [(small if size <= 24 else full).resize((size, size), Image.LANCZOS) for size in SIZES]


def ico(path, colors):
    images = frames(colors)
    images[0].save(path, format="ICO", sizes=[(s, s) for s in SIZES], append_images=images[1:])


SIZES = [256, 128, 64, 48, 40, 32, 24, 20, 16]
EMBER = ((222, 122, 86), (181, 78, 47), (253, 250, 243), (247, 233, 216))
NIGHT = ((52, 49, 46), (26, 25, 23), (236, 142, 104), (201, 100, 66))
PAPER = ((253, 251, 246), (238, 233, 221), (222, 122, 86), (186, 84, 52))
ASH = ((150, 146, 139), (102, 99, 94), (246, 244, 239), (232, 229, 222))

ico("crates/myuri/assets/myuri.ico", EMBER)
ico("crates/myuri/assets/myuri-paused.ico", ASH)

if "--preview" in sys.argv:
    sheet = Image.new("RGB", (1010, 560), (243, 241, 235))
    ImageDraw.Draw(sheet).rectangle((0, 280, 1010, 560), fill=(30, 29, 28))
    for column, colors in enumerate((NIGHT, EMBER, PAPER)):
        images = frames(colors)
        for row in (0, 1):
            x, y = 14 + column * 334, 12 + row * 280
            sheet.paste(images[0], (x, y), images[0])
            for index, dy in ((3, 0), (5, 62), (6, 106), (8, 142)):
                sheet.paste(images[index], (x + 268, y + dy), images[index])
    sheet.save("target/icon-preview.png")
print("ok")
