#!/usr/bin/env bash
# Sourced by the platform build entry points.
set -euo pipefail
MOBILE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$MOBILE_ROOT"
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=line-tables-only
MOBILE_PROFILE="${MOBILE_PROFILE:-release}"
MOBILE_CARGO_FLAGS=(--locked)
case "$MOBILE_PROFILE" in
  release) MOBILE_CARGO_FLAGS+=(--release) ;;
  debug) ;;
  *) echo 'MOBILE_PROFILE must be release or debug' >&2; exit 2 ;;
esac
MOBILE_TARGET_DIR="${CARGO_TARGET_DIR:-$MOBILE_ROOT/target}"
mkdir -p "$MOBILE_TARGET_DIR"
MOBILE_TARGET_DIR="$(cd "$MOBILE_TARGET_DIR" && pwd)"
export CARGO_TARGET_DIR="$MOBILE_TARGET_DIR"
# Generate with the exact UniFFI revision linked into the native library.
cargo build --locked --manifest-path build-support/mobile/uniffi-bindgen/Cargo.toml --target-dir "$MOBILE_TARGET_DIR/uniffi-bindgen"
MOBILE_BINDGEN="$MOBILE_TARGET_DIR/uniffi-bindgen/debug/oink-uniffi-bindgen"
