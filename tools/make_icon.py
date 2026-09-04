#!/usr/bin/env python3
"""Regenerate the app icons from the transparent source logo.

Outputs
-------
resources/AppIcon.ico   BMP-based 32-bit BGRA icon with an alpha channel.
                        This is the format every Windows icon render path
                        (Explorer, taskbar, desktop shortcut) composites
                        transparently. PNG-compressed ICOs are NOT reliably
                        transparent on the desktop, hence the BMP approach.
resources/AppIcon.icns  macOS icon set (requires `iconutil`, macOS only).

Run from the repo root:
    python3 tools/make_icon.py
"""
import io
import struct

from PIL import Image

SRC = "resources/mouse-logo.png"
ICO = "resources/AppIcon.ico"
ICNS = "resources/AppIcon.icns"
ICO_SIZES = [16, 32, 48, 64, 128, 256]

# MouseShare logo on a transparent background. If the source still has a white
# background, flood-fill it out first (4-connected from every edge).
NEAR_WHITE = 240


def transparentify(src: Image.Image) -> Image.Image:
    im = src.convert("RGBA")
    w, h = im.size
    px = im.load()
    # Build a binary "background?" mask via 4-connected flood fill from the edges,
    # only through near-white, fully opaque pixels.
    bg = [[False] * w for _ in range(h)]
    from collections import deque

    q = deque()
    for x in range(w):
        for y in (0, h - 1):
            if px[x, y][0] > NEAR_WHITE and px[x, y][1] > NEAR_WHITE and px[x, y][2] > NEAR_WHITE:
                if not bg[y][x]:
                    bg[y][x] = True
                    q.append((x, y))
    for y in range(h):
        for x in (0, w - 1):
            if px[x, y][0] > NEAR_WHITE and px[x, y][1] > NEAR_WHITE and px[x, y][2] > NEAR_WHITE:
                if not bg[y][x]:
                    bg[y][x] = True
                    q.append((x, y))
    while q:
        x, y = q.popleft()
        for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1)):
            nx, ny = x + dx, y + dy
            if 0 <= nx < w and 0 <= ny < h and not bg[ny][nx]:
                r, g, b = px[nx, ny][:3]
                if r > NEAR_WHITE and g > NEAR_WHITE and b > NEAR_WHITE:
                    bg[ny][nx] = True
                    q.append((nx, ny))
    for y in range(h):
        for x in range(w):
            if bg[y][x]:
                px[x, y] = (px[x, y][0], px[x, y][1], px[x, y][2], 0)
    return im


def make_bmp_icon(im: Image.Image, size: int) -> bytes:
    img = im.resize((size, size), Image.LANCZOS)
    w = h = size
    px = img.load()
    xor = bytearray()
    for y in range(h - 1, -1, -1):  # BMP rows are bottom-up
        for x in range(w):
            r, g, b, a = px[x, y]
            xor += bytes((b, g, r, a))  # BGRA
    and_row = (w + 31) // 32 * 4  # 1-bit AND mask, 4-byte padded rows
    and_mask = b"\x00" * (and_row * h)  # all zeros: alpha channel carries transparency
    bih = struct.pack(
        "<IiiHHIIiiII",
        40,  # biSize
        w,  # biWidth
        h * 2,  # biHeight (XOR + AND)
        1,  # biPlanes
        32,  # biBitCount
        0,  # biCompression = BI_RGB
        len(xor) + len(and_mask),  # biSizeImage
        0, 0, 0, 0,
    )
    return bih + bytes(xor) + and_mask


def build_ico(im: Image.Image) -> bytes:
    entries = []
    data = []
    for s in ICO_SIZES:
        d = make_bmp_icon(im, s)
        entries.append((s, d))
        data.append(d)
    out = bytearray()
    out += struct.pack("<HHH", 0, 1, len(entries))
    offset = 6 + 16 * len(entries)
    for s, d in entries:
        b = 0 if s >= 256 else s
        out += struct.pack("<BBBBHHII", b, b, 0, 0, 1, 32, len(d), offset)
        offset += len(d)
    for d in data:
        out += d
    return bytes(out)


def build_icns(im: Image.Image) -> bytes:
    specs = {
        "icon_16x16.png": 16,
        "icon_16x16@2x.png": 32,
        "icon_32x32.png": 32,
        "icon_32x32@2x.png": 64,
        "icon_128x128.png": 128,
        "icon_128x128@2x.png": 256,
        "icon_256x256.png": 256,
        "icon_256x256@2x.png": 512,
        "icon_512x512.png": 512,
        "icon_512x512@2x.png": 1024,
    }
    import os
    import subprocess

    iconset = "/tmp/AppIcon.iconset"
    os.makedirs(iconset, exist_ok=True)
    for name, size in specs.items():
        im.resize((size, size), Image.LANCZOS).save(f"{iconset}/{name}")
    subprocess.run(
        ["iconutil", "--convert", "icns", "--output", ICNS, iconset], check=True
    )
    return open(ICNS, "rb").read()


def main() -> None:
    src = Image.open(SRC)
    if src.mode != "RGBA":
        im = transparentify(src)
    else:
        # Already has alpha — but ensure the corners are actually transparent.
        im = transparentify(src)
    with open(ICO, "wb") as f:
        f.write(build_ico(im))
    print(f"wrote {ICO} ({len(build_ico(im))} bytes, BMP 32-bit BGRA w/ alpha)")
    try:
        build_icns(im)
        print(f"wrote {ICNS}")
    except Exception as e:  # iconutil is macOS-only
        print(f"skip icns ({e})")


if __name__ == "__main__":
    main()
