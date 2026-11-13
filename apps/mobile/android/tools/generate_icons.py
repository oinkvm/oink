#!/usr/bin/env python3
"""Generate Oink Android launcher artwork from the shared B2 mark SVGs.

The mark sources have no tile, rounded card, border, shadow, or texture.
Android's adaptive icon receives a separate solid-color background layer and
the transparent foreground contains only a reduced, centered Oink mark. The
foreground canvas is 108 dp in Android's adaptive-icon model; the launcher
only guarantees the central 66 dp safe area across circle, square, and
squircle masks.  MARK_SCALE is deliberately explicit so future artwork
changes are reviewed against that geometry instead of being hand-tuned PNGs.
The legacy PNG is also a square solid-color tile with the same reduced mark.
rsvg-convert is intentionally the only rasterizer used here.
"""
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
MARK_LIGHT = ROOT / "src/main/icon/mark.svg"
MARK_DARK = ROOT / "src/main/icon/mark-dark.svg"
RES = ROOT / "app/src/main/res"
ACCENTS = {
    "pink": "#FF6B8E",
    "orange": "#FF8A3D",
    "violet": "#8B6CF6",
    "blue": "#3D8BFD",
    "teal": "#14B8A6",
}

# B2 mark artwork is authored in a 64 × 64 viewBox. Android's adaptive icon
# foreground is conceptually 108 × 108 dp, with only a 66 × 66 dp cross-mask
# safe area guaranteed by launchers. The measured mark bounds are approximately
# x=6..58, y=5..59 in the source viewBox. A 0.52 scale gives a 41–44%
# canvas keyline and leaves a measured margin inside the 66 dp safe circle
# while retaining the mark's visual weight.
MARK_SCALE = 0.52
MARK_SOURCE_CENTER = (32.0, 32.0)
MARK_CANVAS_CENTER = (32.0, 32.0)


def centered_safe_mark(svg: str) -> str:
    match = re.fullmatch(r'(?s)(<svg[^>]*>)(.*)(</svg>)', svg.strip())
    if not match:
        raise ValueError("mark SVG must have one root element")
    dx = MARK_CANVAS_CENTER[0] - MARK_SOURCE_CENTER[0] * MARK_SCALE
    dy = MARK_CANVAS_CENTER[1] - MARK_SOURCE_CENTER[1] * MARK_SCALE
    inner = match.group(2)
    return f'{match.group(1)}<g transform="translate({dx:.4f} {dy:.4f}) scale({MARK_SCALE:.4f})">{inner}</g>{match.group(3)}'


def themed_svg(dark: bool, accent: str, foreground: bool = False, monochrome: bool = False) -> str:
    svg = (MARK_DARK if dark else MARK_LIGHT).read_text()
    tile = "#000000" if dark else "#FFFFFF"
    ink = "#FFFFFF" if dark else "#000000"
    if monochrome:
        svg = svg.replace("#FF6F91", "#000000").replace("#1B1E28", "#000000").replace("#F2F3F5", "#000000")
    else:
        svg = svg.replace("#FF6F91", accent)
        svg = svg.replace("#1B1E28", ink).replace("#F2F3F5", ink)
    svg = centered_safe_mark(svg)
    if not foreground:
        # Legacy icons must be a square solid tile. Android/launcher applies
        # any device-specific mask outside this artwork.
        svg = re.sub(
            r'(<svg[^>]*>)',
            rf'\1<rect x="0" y="0" width="64" height="64" fill="{tile}"/>',
            svg,
            count=1,
        )
    return svg


def render(svg: str, output: Path, size: int = 432) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    source = output.with_suffix(".svg")
    source.write_text(svg)
    subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size), "-o", str(output), str(source)], check=True)
    source.unlink()


def main() -> None:
    if not shutil.which("rsvg-convert"):
        raise SystemExit("rsvg-convert is required")
    foreground_dir = RES / "drawable-nodpi"
    mono = foreground_dir / "oink_monochrome.png"
    render(themed_svg(False, ACCENTS["pink"], foreground=True, monochrome=True), mono)
    for name, color in ACCENTS.items():
        for dark in (False, True):
            suffix = f"{name}_{'dark' if dark else 'light'}"
            render(themed_svg(dark, color, foreground=True), foreground_dir / f"oink_icon_{suffix}_foreground.png")
            render(themed_svg(dark, color, foreground=False), foreground_dir / f"oink_icon_{suffix}_legacy.png")
            mipmap = RES / "mipmap-anydpi-v26" / f"ic_launcher_{suffix}.xml"
            mipmap.parent.mkdir(parents=True, exist_ok=True)
            mipmap.write_text(f'''<?xml version="1.0" encoding="utf-8"?>
<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">
    <background android:drawable="@color/oink_icon_{suffix}_background" />
    <foreground android:drawable="@drawable/oink_icon_{suffix}_foreground" />
    <monochrome android:drawable="@drawable/oink_monochrome" />
</adaptive-icon>
''')
            colors = RES / "values" / f"oink_icon_{suffix}.xml"
            colors.parent.mkdir(parents=True, exist_ok=True)
            colors.write_text(f'''<?xml version="1.0" encoding="utf-8"?>
<resources><color name="oink_icon_{suffix}_background">{('#000000' if dark else '#FFFFFF')}</color></resources>
''')


if __name__ == "__main__":
    main()
