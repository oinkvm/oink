#!/usr/bin/env python3
"""Supplement parser tests with ASCII/UTF-16 checks of built production artifacts.

Usage: python3 build-support/check-product-boundary.py path/to/release/binary [assets ...]
Pass files or directories. This never executes a backend or changes an installation.
"""
import argparse
import json
from pathlib import Path

FORBIDDEN = (
    "set_debug_delay", "input_trace_status", "set_input_trace", "mock_simulate",
    "OINK_UI_MOCK", "OINK_UI_VIEW", "OINK_PASTEBOARD", "OINK_CURSOR_TIMING",
    "OINK_DND_VISIBLE", "OINK_WIN_CURSOR_HIDE", "OINK_REQUIRE_INPUT_SERVICE",
    "OINK_NET_QOS", "OINK_NET_SERVICE", "OINK_NET_INTERACTIVE_DSCP",
    "OINK_NET_LOW_DSCP", "OINK_NET_BULK_TARGET_MS", "OINK_NET_BULK_PACE",
    "OINK_PORT", "OINK_BULK_PORT", "OINK_BEACON_PORTS", "OINK_IPC_PORT",
    "OINK_MDNS", "OINK_BEACON", "OINK_CALIB_PORT", "OINK_NAME",
    "OINK_DOWNLOADS", "OINK_LOG", "OinkFPClock ", "OinkFP ",
    "oink-probe", "menu_probe launch=", "synthetic circling is limited",
)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", nargs="+", type=Path)
    args = parser.parse_args()
    files = []
    for path in args.artifacts:
        if not path.exists():
            parser.error(f"missing artifact: {path}")
        files.extend([path] if path.is_file() else (p for p in path.rglob("*") if p.is_file()))
    if not files:
        parser.error("no artifact files")
    hits = []
    for path in files:
        data = path.read_bytes()
        for marker in FORBIDDEN:
            if any(marker.encode(encoding) in data for encoding in ("utf-8", "utf-16-le")):
                hits.append({"artifact": str(path), "marker": marker})
    print(json.dumps({"files": len(files), "violations": hits}, indent=2))
    return bool(hits)


if __name__ == "__main__":
    raise SystemExit(main())
