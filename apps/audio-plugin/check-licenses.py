#!/usr/bin/env python3
"""Native dependency companion to cargo-deny; same allowlist/scoped exceptions."""
import hashlib
import json
from pathlib import Path
import tomllib
import sys

def require(condition, message):
    if not condition:
        raise SystemExit(message)


here = Path(__file__).resolve().parent
policy = tomllib.loads((here.parent.parent / "deny.toml").read_text())["licenses"]
dep = json.loads((here / "dependencies.json").read_text())
allowed = set(policy["allow"])
for exception in policy.get("exceptions", []):
    if exception["name"] == dep["name"]:
        allowed.update(exception["allow"])
for filename, license in dep["licenses"].items():
    require(license["spdx"] in allowed, f"{dep['name']}: {license['spdx']} not allowed")
    require(hashlib.sha256((here / "licenses" / filename).read_bytes()).hexdigest() == license["sha256"], filename)
    downloaded = (Path(sys.argv[1]) if len(sys.argv) > 1 else here / "build") / "_deps" / "aspl-src" / filename
    if downloaded.exists():
        require(downloaded.read_bytes() == (here / "licenses" / filename).read_bytes(), filename)
cmake = (here / "CMakeLists.txt").read_text()
require(dep["revision"] in cmake and dep["archive_sha256"] in cmake, "CMake source pin mismatch")
print(f"{dep['name']} {dep['version']}: MIT + scoped Apple-MIT; checksums and notices verified")
