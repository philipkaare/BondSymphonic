"""Render the BondSymphonic pixel-art logo to SVG and PNG (stdlib only).

Usage: python assets/make_logo.py
Writes assets/logo.svg, assets/logo.png (512 px), assets/logo-256.png, assets/logo-64.png, assets/logo-32.png,
assets/logo.ico (256/64/32 PNG-compressed entries, for the Windows executable) and
crates/ide/cpp/LogoData.h (the 256 px PNG as a byte array, for the window icon and About box).
"""
from __future__ import annotations

import struct
import zlib
from pathlib import Path

SIZE = 32  # grid is SIZE x SIZE

# Palette: char -> RGBA. '.' = background (barrel circle or transparent outside it).
PALETTE = {
    ".": None,
    "k": (0x12, 0x12, 0x16, 255),  # tuxedo / hair
    "K": (0x2a, 0x2a, 0x32, 255),  # tuxedo highlight
    "s": (0xe9, 0xb9, 0x8b, 255),  # skin
    "S": (0xc9, 0x93, 0x66, 255),  # skin shadow
    "w": (0xf7, 0xf7, 0xf2, 255),  # shirt
    "r": (0xc8, 0x24, 0x2c, 255),  # bow tie
    "g": (0xf4, 0xc4, 0x4e, 255),  # baton / notes (gold)
    "G": (0xff, 0xe6, 0x9a, 255),  # baton tip highlight
    "e": (0xf7, 0xf7, 0xf2, 255),  # eye white
    "p": (0x12, 0x12, 0x16, 255),  # pupil
}

BG_OUTER = (0x0b, 0x14, 0x2b, 255)   # deep navy barrel
BG_RING = (0x16, 0x2a, 0x55, 255)    # inner ring
BG_RIFLE = (0xc9, 0xd3, 0xe6, 255)   # pale rifling ring
BG_SIGHT = (0xf2, 0xf5, 0xfa, 255)   # crosshair ticks
BG_INNER = (0x1f, 0x3b, 0x74, 255)   # centre glow

# 32 columns per row. Agent faces the viewer, baton raised to the upper right.
ART = [
    "................................",  # 0
    ".......................G........",  # 1
    ".......................G........",  # 2
    ".......................g........",  # 3
    ".......................g........",  # 4
    "....g.......kkkkkk.....g........",  # 5
    "....g......kkkkkkkk....g........",  # 6
    "...gg......kssssssk....g........",  # 7
    "...gg......ksepsepsk..ss........",  # 8
    "...........ksssssssk..ss........",  # 9
    "............ssssss...kkg........",  # 10
    ".............SssS....kk.........",  # 11
    ".........kkkkkrrrrkkkkk.........",  # 12
    "........kkkkkKwwwwKkkkk.........",  # 13
    ".......kkkkkkKwwwwKkkkk.........",  # 14
    ".......kkkkkkkKwwKkkkkk.........",  # 15
    ".......kkkkkkkKwwKkkkkk.........",  # 16
    ".......kkkkkkkkwwkkkkkk.........",  # 17
    ".......kkkkkkkkKkkkkkkk.........",  # 18
    ".......ss.kkkkkkkkkkkk.....g....",  # 19
    ".......ss.kkkkkkkkkkkk.....g....",  # 20
    "..........kkkkkkkkkkkk....gg....",  # 21
    "..........kkkkk..kkkkk....gg....",  # 22
    "..........kkkk....kkkk..........",  # 23
    "..........kkkk....kkkk..........",  # 24
    "..........kkkk....kkkk..........",  # 25
    "..........kkkk....kkkk..........",  # 26
    "..........kkkk....kkkk..........",  # 27
    ".........kkkkk....kkkkk.........",  # 28
    ".........kkkkk....kkkkk.........",  # 29
    "................................",  # 30
    "................................",  # 31
]

assert len(ART) == SIZE and all(len(r) == SIZE for r in ART), "grid must be 32x32"


def background(x: int, y: int) -> tuple[int, int, int, int] | None:
    """Gun-barrel style concentric circle; None outside the circle (transparent)."""
    cx = cy = (SIZE - 1) / 2
    d2 = (x - cx) ** 2 + (y - cy) ** 2
    r = (SIZE / 2) - 0.5
    if d2 > r * r:
        return None
    # Gunsight crosshair: four bold ticks from the rim inward, leaving the centre open
    # so the figure stays readable. 2 px wide because the 32-grid centre is at 15.5.
    if (15 <= x <= 16 or 15 <= y <= 16) and d2 > (r - 9.0) ** 2:
        return BG_SIGHT
    if d2 > (r - 2.0) ** 2:
        return BG_OUTER
    if d2 > (r - 3.0) ** 2:
        return BG_RIFLE
    if d2 > (r - 5.0) ** 2:
        return BG_RING
    return BG_INNER


def pixels() -> list[list[tuple[int, int, int, int] | None]]:
    grid = []
    for y, row in enumerate(ART):
        out = []
        for x, ch in enumerate(row):
            col = PALETTE[ch]
            out.append(col if col is not None else background(x, y))
        grid.append(out)
    return grid


def write_svg(path: Path, grid) -> None:
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SIZE} {SIZE}" '
        f'width="{SIZE * 16}" height="{SIZE * 16}" shape-rendering="crispEdges">'
    ]
    for y, row in enumerate(grid):
        for x, c in enumerate(row):
            if c is None:
                continue
            parts.append(f'<rect x="{x}" y="{y}" width="1" height="1" fill="#{c[0]:02x}{c[1]:02x}{c[2]:02x}"/>')
    parts.append("</svg>")
    path.write_text("\n".join(parts), encoding="utf-8")


def write_png(path: Path, grid, scale: int) -> None:
    w = h = SIZE * scale
    raw = bytearray()
    for y in range(h):
        raw.append(0)  # filter type 0
        row = grid[y // scale]
        for x in range(w):
            c = row[x // scale]
            raw.extend(c if c is not None else (0, 0, 0, 0))

    def chunk(tag: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b"")
    path.write_bytes(png)


def write_ico(path: Path, pngs: list[tuple[int, bytes]]) -> None:
    """ICO container with PNG-compressed images (supported since Windows Vista)."""
    header = struct.pack("<HHH", 0, 1, len(pngs))
    offset = len(header) + 16 * len(pngs)
    entries = b""
    for size, data in pngs:
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    path.write_bytes(header + entries + b"".join(d for _, d in pngs))


def write_header(path: Path, png: bytes) -> None:
    lines = ["// Generated by assets/make_logo.py — do not edit. The 256 px logo as PNG bytes.",
             "#pragma once", "#include <cstddef>", "", "inline constexpr unsigned char kLogoPng256[] = {"]
    for i in range(0, len(png), 16):
        lines.append("    " + ", ".join(f"0x{b:02x}" for b in png[i:i + 16]) + ",")
    lines += ["};", f"inline constexpr std::size_t kLogoPng256Size = {len(png)};", ""]
    path.write_text(chr(10).join(lines), encoding="utf-8")


def main() -> None:
    here = Path(__file__).resolve().parent
    grid = pixels()
    write_svg(here / "logo.svg", grid)
    write_png(here / "logo.png", grid, 16)
    write_png(here / "logo-256.png", grid, 8)
    write_png(here / "logo-64.png", grid, 2)
    write_png(here / "logo-32.png", grid, 1)
    pngs = [(n, (here / f"logo-{n}.png").read_bytes()) for n in (256, 64, 32)]
    write_ico(here / "logo.ico", pngs)
    write_header(here.parent / "crates" / "ide" / "cpp" / "LogoData.h", pngs[0][1])
    print("wrote logo.svg, logo.png, logo-256.png, logo-64.png, logo-32.png, logo.ico, crates/ide/cpp/LogoData.h")


if __name__ == "__main__":
    main()
