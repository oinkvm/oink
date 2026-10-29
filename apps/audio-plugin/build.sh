#!/bin/bash
# Build/test only. Release packaging signs the result inside-out.
set -euo pipefail
cd "$(dirname "$0")"
[[ $# == 2 && "$2" =~ ^[a-f0-9]{64}$ ]] || { echo 'Usage: build.sh <build directory> <source-tree SHA256>' >&2; exit 2; }
cmake -S . -B "$1" -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_ARCHITECTURES=arm64 -DOINK_PLUGIN_GIT_SHA="$2"
python3 check-licenses.py "$1"
cmake --build "$1" --parallel 2
ctest --test-dir "$1" --output-on-failure
