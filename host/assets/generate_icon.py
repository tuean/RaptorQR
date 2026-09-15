#!/usr/bin/env python3
"""Generate assets/RaptorQR.icns — a QR-styled app icon (needs Pillow + iconutil).

Run: python3 host/assets/generate_icon.py
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

from PIL import Image, ImageDraw

ASSETS = Path(__file__).resolve().parent
ICONSET = ASSETS / "RaptorQR.iconset"
SIZE = 1024
BG = (13, 17, 23)          # app background
MODULE = (240, 246, 252)   # QR modules
ACCENT = (88, 166, 255)    # brand blue
GRID = 21                  # 21x21 modules: QR version 1 proportions


def rounded_background() -> Image.Image:
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    draw.rounded_rectangle(
        (0, 0, SIZE - 1, SIZE - 1), radius=int(SIZE * 0.22), fill=BG
    )
    return img


def modules() -> list[tuple[int, int]]:
    """Deterministic dark-module pattern (finder eyes + pseudo-random fill)."""
    dark: list[tuple[int, int]] = []
    finders = [(0, 0), (GRID - 7, 0), (0, GRID - 7)]

    def in_finder(x: int, y: int) -> bool:
        for fx, fy in finders:
            if fx <= x < fx + 7 and fy <= y < fy + 7:
                lx, ly = x - fx, y - fy
                edge = lx in (0, 6) or ly in (0, 6)
                core = 2 <= lx <= 4 and 2 <= ly <= 4
                if edge or core:
                    dark.append((x, y))
                return True
        return False

    state = 0x2F6E2B1
    for y in range(GRID):
        for x in range(GRID):
            if in_finder(x, y):
                continue
            # keep the finder separators and timing patterns light
            if (x == 6 and y < 8) or (y == 6 and x < 8):
                continue
            state = (state * 1664525 + 1013904223) & 0xFFFFFFFF
            if (state >> 17) % 100 < 46:
                dark.append((x, y))
    return dark


def build_icon() -> Image.Image:
    img = rounded_background()
    draw = ImageDraw.Draw(img)

    margin = int(SIZE * 0.17)
    usable = SIZE - margin * 2
    cell = usable / GRID

    for index, (x, y) in enumerate(modules()):
        x0 = margin + x * cell
        y0 = margin + y * cell
        # accent the bottom-right quadrant so it reads as a brand mark
        colour = ACCENT if (x >= GRID // 2 and y >= GRID // 2) else MODULE
        draw.rounded_rectangle(
            (x0, y0, x0 + cell - cell * 0.12, y0 + cell - cell * 0.12),
            radius=cell * 0.18,
            fill=colour,
        )
    return img


def main() -> None:
    icon = build_icon()
    icon.save(ASSETS / "RaptorQR.png")

    if ICONSET.exists():
        shutil.rmtree(ICONSET)
    ICONSET.mkdir()

    for size in (16, 32, 128, 256, 512):
        icon.resize((size, size), Image.LANCZOS).save(ICONSET / f"icon_{size}x{size}.png")
        icon.resize((size * 2, size * 2), Image.LANCZOS).save(
            ICONSET / f"icon_{size}x{size}@2x.png"
        )

    subprocess.run(
        ["iconutil", "-c", "icns", str(ICONSET), "-o", str(ASSETS / "RaptorQR.icns")],
        check=True,
    )
    shutil.rmtree(ICONSET)
    print(f"wrote {ASSETS / 'RaptorQR.icns'} ({(ASSETS / 'RaptorQR.icns').stat().st_size} bytes)")


if __name__ == "__main__":
    main()
