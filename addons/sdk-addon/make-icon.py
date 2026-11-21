"""Draws icon.png, the SDK Add-on's sidebar picture: a white </> on a transparent background.

64 x 64 pixels. Toolboks uses only the transparency, so the shape is all that matters. The
strokes follow the host's icon style: 1.8 px on a 24 px grid (4.8 px here), round caps and joins,
with an empty margin like the built-in icons. Python 3, standard library only:

    python make-icon.py        (writes icon.png beside this file)
"""

import math
import os
import struct
import zlib

SIDE = 64
SCALE = SIDE / 24
HALF_WIDTH = 1.8 * SCALE / 2
# Each stroke is a polyline on the 24 px grid.
STROKES = [
    [(7.0, 7.5), (2.5, 12.0), (7.0, 16.5)],  # <
    [(17.0, 7.5), (21.5, 12.0), (17.0, 16.5)],  # >
    [(14.0, 5.0), (10.0, 19.0)],  # /
]
SEGMENTS = [
    ((ax * SCALE, ay * SCALE), (bx * SCALE, by * SCALE))
    for line in STROKES
    for (ax, ay), (bx, by) in zip(line, line[1:])
]
SUBSAMPLES = 4


def distance(px, py, a, b):
    """Distance from (px, py) to the segment a-b (round ends)."""
    (ax, ay), (bx, by) = a, b
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))


def alpha(x, y):
    """Share of the pixel at (x, y) the strokes cover, 0 to 255."""
    inside = 0
    for i in range(SUBSAMPLES):
        for j in range(SUBSAMPLES):
            px = x + (i + 0.5) / SUBSAMPLES
            py = y + (j + 0.5) / SUBSAMPLES
            if any(distance(px, py, a, b) <= HALF_WIDTH for a, b in SEGMENTS):
                inside += 1
    return round(255 * inside / SUBSAMPLES**2)


def chunk(kind, data):
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))


def png():
    rows = bytearray()
    for y in range(SIDE):
        rows.append(0)  # filter: none
        for x in range(SIDE):
            a = alpha(x, y)
            rows += bytes((255, 255, 255, a) if a else (0, 0, 0, 0))
    header = struct.pack(">IIBBBBB", SIDE, SIDE, 8, 6, 0, 0, 0)  # 8-bit RGBA
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(bytes(rows), 9))
        + chunk(b"IEND", b"")
    )


if __name__ == "__main__":
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "icon.png")
    with open(path, "wb") as out:
        out.write(png())
    print(f"wrote {path}")
