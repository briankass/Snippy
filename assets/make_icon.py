import math, sys
from PIL import Image, ImageDraw, ImageFilter, ImageChops
N = 1024

def lerp(a, b, t): return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(len(a)))

def render(simple=False):
    c0, c1 = (54, 214, 255), (124, 77, 255)
    grad = Image.new("RGBA", (N, N)); px = grad.load()
    for y in range(N):
        for x in range(N):
            px[x, y] = lerp(c0, c1, (x + y) / (2 * N - 2)) + (255,)
    pad, r = (20, 200) if simple else (40, 230)
    mask = Image.new("L", (N, N), 0)
    ImageDraw.Draw(mask).rounded_rectangle([pad, pad, N - pad, N - pad], r, fill=255)
    tile = Image.new("RGBA", (N, N), (0, 0, 0, 0)); tile.paste(grad, (0, 0), mask)
    # glassy sheen
    sheen = Image.new("L", (N, N), 0); sd = ImageDraw.Draw(sheen)
    for y in range(N // 2):
        sd.line([(0, y), (N, y)], fill=int(70 * (1 - y / (N / 2)) ** 1.6))
    sheen = ImageChops.multiply(sheen, mask)
    tile = Image.alpha_composite(tile, Image.merge("RGBA", (*Image.new("RGB", (N, N), (255, 255, 255)).split(), sheen)))
    if not simple:
        brk = Image.new("RGBA", (N, N), (0, 0, 0, 0)); bd = ImageDraw.Draw(brk)
        m, L, w, col = 170, 140, 44, (255, 255, 255, 115)
        for (x, y, dx, dy) in [(m, m, 1, 1), (N - m, m, -1, 1), (m, N - m, 1, -1)]:
            bd.line([(x, y + dy * L), (x, y), (x + dx * L, y)], fill=col, width=w, joint="curve")
            for p in [(x, y + dy * L), (x + dx * L, y), (x, y)]:
                bd.ellipse([p[0] - w / 2, p[1] - w / 2, p[0] + w / 2, p[1] + w / 2], fill=col)
        tile = Image.alpha_composite(tile, brk)

    k = 1.32 if simple else 1.0           # bolder for small sizes
    sc = Image.new("RGBA", (N, N), (0, 0, 0, 0)); d = ImageDraw.Draw(sc); W = (255, 255, 255, 255)
    cx, cy = N * 0.47, N * (0.44 if not simple else 0.42)
    def blade(angle_deg, length, width):
        a = math.radians(angle_deg); ux, uy = math.cos(a), math.sin(a); px_, py_ = -uy, ux
        tip = (cx + ux * length, cy + uy * length); back = (cx - ux * 50, cy - uy * 50)
        d.polygon([(back[0] + px_ * width, back[1] + py_ * width), tip,
                   (back[0] - px_ * width * 0.35, back[1] - py_ * width * 0.35)], fill=W)
    blade(-52, 300 * k, 40 * k); blade(-128, 300 * k, 40 * k)
    R, ring_w, arm_w, dist = 80 * k, 44 * k, 50 * k, 225 * k
    for side in (-1, 1):
        a = math.radians(90 + side * 34)
        rx, ry = cx + math.cos(a) * dist, cy + math.sin(a) * dist
        # arm stops at the ring's outer edge
        ex, ey = cx + math.cos(a) * (dist - R), cy + math.sin(a) * (dist - R)
        d.line([(cx, cy), (ex, ey)], fill=W, width=int(arm_w))
        d.ellipse([rx - R, ry - R, rx + R, ry + R], outline=W, width=int(ring_w))
    d.ellipse([cx - 38 * k, cy - 38 * k, cx + 38 * k, cy + 38 * k], fill=W)
    if not simple:
        d.ellipse([cx - 15, cy - 15, cx + 15, cy + 15], fill=(110, 90, 255, 255))
    shadow = Image.new("RGBA", (N, N), (20, 10, 60, 0))
    shadow.putalpha(sc.split()[3].point(lambda v: int(v * 0.45)))
    shadow = shadow.filter(ImageFilter.GaussianBlur(18)).transform((N, N), Image.AFFINE, (1, 0, -8, 0, 1, -14))
    tile = Image.alpha_composite(tile, shadow); tile = Image.alpha_composite(tile, sc)
    # record dot in the bottom-right corner (where the 4th bracket would be)
    rd = ImageDraw.Draw(tile)
    R = 92 if simple else 78; x, y = N - (215 if simple else 200), N - (215 if simple else 200)
    rd.ellipse([x - R - 16, y - R - 16, x + R + 16, y + R + 16], fill=(255, 255, 255, 255))
    rd.ellipse([x - R, y - R, x + R, y + R], fill=(240, 60, 70, 255))
    return tile

out = sys.argv[1]
full, simple = render(False), render(True)
full.save(out + "/snippy-1024.png")
full.resize((256, 256), Image.LANCZOS).save(out + "/snippy.png")
# ICO: detailed art for big sizes, simplified art for small ones
frames = {s: (simple if s <= 32 else full).resize((s, s), Image.LANCZOS) for s in [16, 20, 24, 32, 40, 48, 64, 128, 256]}
frames[256].save(out + "/snippy.ico", format="ICO", sizes=[(s, s) for s in frames], append_images=[frames[s] for s in frames if s != 256])
# tray icon: simplified art
simple.resize((64, 64), Image.LANCZOS).save(out + "/snippy-tray.png")
bg = Image.new("RGBA", (1200, 420), (40, 40, 48, 255)); bg.paste(full.resize((380, 380)), (20, 20), full.resize((380, 380)))
x = 440
for s in [128, 64, 48, 32, 24, 16]:
    t = frames[s]; bg.paste(t, (x, 200 - s // 2), t); x += s + 30
light = Image.new("RGBA", (300, 80), (235, 235, 240, 255)); x = 10
for s in [32, 24, 16]:
    t = frames[s]; light.paste(t, (x, 40 - s // 2), t); x += s + 40
bg.paste(light, (880, 330))
bg.save(out + "/icon_preview.png"); print("ok")
