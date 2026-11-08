#!/usr/bin/env python3
"""Render the Oink VI mark for native Paste as File menus.

The SVG inputs are the approved b2 brand assets. rsvg-convert is intentionally
used instead of a hand-drawn bitmap so every checked-in raster keeps the same
mark geometry and transparent background.
"""
from __future__ import annotations

import argparse
from contextlib import nullcontext
import struct
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MAC_RESOURCES = ROOT / "apps/finder-sync/Resources"
WINDOWS_RESOURCES = ROOT / "packaging/windows"
SIZES = (16, 20, 24, 32, 40, 48, 64)


def render(source: Path, destination: Path, size: int) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["rsvg-convert", "-w", str(size), "-h", str(size), "-o", str(destination), str(source)],
        check=True,
    )


def write_ico(path: Path, pngs: list[Path]) -> None:
    # Windows accepts PNG payloads in an ICO. Keep the directory ordered from
    # the requested menu size through the high-DPI variants.
    blobs = [item.read_bytes() for item in pngs]
    header = struct.pack("<HHH", 0, 1, len(blobs))
    entries = bytearray()
    offset = 6 + 16 * len(blobs)
    for size, blob in zip(SIZES, blobs):
        entries.extend(struct.pack(
            "<BBBBHHII",
            size if size < 256 else 0,
            size if size < 256 else 0,
            0,
            0,
            1,
            32,
            len(blob),
            offset,
        ))
        offset += len(blob)
    path.write_bytes(header + entries + b"".join(blobs))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="fail if generated files differ")
    args = parser.parse_args()
    generated: list[tuple[Path, bytes]] = []
    with (tempfile.TemporaryDirectory(prefix="oink-paste-menu-icons-") if args.check else nullcontext(ROOT)) as temporary:
        output_root = Path(temporary) if args.check else ROOT
        for source_name, stem in (("mark.svg", "OinkPasteMenu"), ("mark-reversed.svg", "OinkPasteMenuDark")):
            source = MAC_RESOURCES / source_name
            pngs: list[Path] = []
            for size in SIZES:
                destination = output_root / "packaging/windows" / f".{stem}-{size}.png"
                render(source, destination, size)
                pngs.append(destination)
            icon = output_root / "packaging/windows" / f"{stem}.ico"
            write_ico(icon, pngs)
            generated.append((WINDOWS_RESOURCES / f"{stem}.ico", icon.read_bytes()))
            for size, png in zip(SIZES, pngs):
                if size in (16, 32):
                    destination = output_root / "apps/finder-sync/Resources" / f"{stem}{'@2x' if size == 32 else ''}.png"
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_bytes(png.read_bytes())
                    generated.append((MAC_RESOURCES / destination.name, destination.read_bytes()))
                png.unlink()
    for path, data in generated:
        if args.check and path.read_bytes() != data:
            raise SystemExit(f"generated file differs from current SVG: {path}")


if __name__ == "__main__":
    main()
