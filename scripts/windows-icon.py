#!/usr/bin/env python3
"""Builds packaging/windows/bars.ico from the PNG app icons.

The 16 and 24 px sizes (Explorer lists, the taskbar at 100%) are scaled
down from the 256 px icon with macOS's `sips`, so run this on a Mac.
"""

import struct
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ICONS = ROOT / "packaging/linux/icons"
OUT = ROOT / "packaging/windows/bars.ico"
SIZES = [16, 24, 32, 48, 64, 128, 256]


def png(size: int, tmp: Path) -> bytes:
    src = ICONS / f"{size}x{size}/xyz.hink.Bars.png"
    if src.exists():
        return src.read_bytes()
    dst = tmp / f"{size}.png"
    subprocess.run(
        ["sips", "-z", str(size), str(size), str(ICONS / "256x256/xyz.hink.Bars.png"), "--out", str(dst)],
        check=True,
        capture_output=True,
    )
    return dst.read_bytes()


def main() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        images = [png(s, Path(tmp)) for s in SIZES]
    # ICONDIR, then one ICONDIRENTRY per image; PNG-compressed entries.
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries = b""
    for size, data in zip(SIZES, images):
        dim = 0 if size == 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_bytes(header + entries + b"".join(images))
    print(f"wrote {OUT.relative_to(ROOT)} ({', '.join(map(str, SIZES))} px)")


if __name__ == "__main__":
    main()
