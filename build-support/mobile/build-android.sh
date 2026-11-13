#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/common.sh"
# cargo-ndk finds the compiler from the SDK, but Opus's CMake build also
# needs the NDK root to select android.toolchain.cmake.
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
if [[ -z "$ANDROID_NDK_HOME" ]]; then
  ANDROID_NDK_HOME="$(find "${ANDROID_HOME:?Set ANDROID_HOME or ANDROID_NDK_HOME}/ndk" -mindepth 1 -maxdepth 1 -type d | sort -V | tail -1)"
fi
export ANDROID_PLATFORM=android-30
MOBILE_ANDROID_OUT="${MOBILE_ANDROID_OUT:-$MOBILE_ROOT/apps/mobile/android}"
mkdir -p "$MOBILE_ANDROID_OUT/build/generated/oink"
# Ship both the physical phone and emulator slices by default. Override this
# only for a constrained local build with MOBILE_ANDROID_ABIS="arm64-v8a".
read -r -a mobile_abis <<< "${MOBILE_ANDROID_ABIS:-arm64-v8a x86_64}"
mobile_ndk_args=()
for abi in "${mobile_abis[@]}"; do mobile_ndk_args+=(-t "$abi"); done
cargo ndk "${mobile_ndk_args[@]}" -P 30 -o "$MOBILE_ANDROID_OUT/app/src/main/jniLibs" \
  rustc -p oink-mobile-ffi --lib "${MOBILE_CARGO_FLAGS[@]}" --crate-type cdylib
# cargo-ndk only copies artifacts reported as rebuilt. Refresh even when Cargo
# returns Fresh, since the previous package copy was stripped after generation.
for abi in "${mobile_abis[@]}"; do
  case "$abi" in
    arm64-v8a) triple=aarch64-linux-android ;;
    x86_64) triple=x86_64-linux-android ;;
    *) echo "Unsupported mobile ABI: $abi" >&2; exit 2 ;;
  esac
  mkdir -p "$MOBILE_ANDROID_OUT/app/src/main/jniLibs/$abi"
  cp "$MOBILE_TARGET_DIR/$triple/$MOBILE_PROFILE/liboink_mobile_ffi.so" "$MOBILE_ANDROID_OUT/app/src/main/jniLibs/$abi/"
  # oink-aec3 contains C++ code, so the Rust cdylib depends on the NDK's
  # shared libc++ runtime. Package the matching ABI beside the cdylib; the
  # Android linker does not provide libc++_shared.so system-wide.
  mobile_cxx_shared="$(find "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt" -type f -path "*/sysroot/usr/lib/$triple/libc++_shared.so" -print -quit)"
  if [[ -z "$mobile_cxx_shared" ]]; then
    echo "Missing libc++_shared.so for Android ABI $abi ($triple)" >&2
    exit 2
  fi
  cp "$mobile_cxx_shared" "$MOBILE_ANDROID_OUT/app/src/main/jniLibs/$abi/"
done
"$MOBILE_BINDGEN" generate --library \
  "$MOBILE_ANDROID_OUT/app/src/main/jniLibs/${mobile_abis[0]}/liboink_mobile_ffi.so" \
  --language kotlin --no-format --out-dir "$MOBILE_ANDROID_OUT/build/generated/oink"
if [[ "$MOBILE_PROFILE" == release ]]; then
  mobile_ndk_home="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
  if [[ -z "$mobile_ndk_home" ]]; then
    mobile_ndk_home="$(find "${ANDROID_HOME:?Set ANDROID_HOME or ANDROID_NDK_HOME}/ndk" -mindepth 1 -maxdepth 1 -type d | sort -V | tail -1)"
  fi
  mobile_strip=("$mobile_ndk_home"/toolchains/llvm/prebuilt/*/bin/llvm-strip)
  for abi in "${mobile_abis[@]}"; do
    "${mobile_strip[0]}" --strip-unneeded "$MOBILE_ANDROID_OUT/app/src/main/jniLibs/$abi/liboink_mobile_ffi.so"
  done
fi
for abi in "${mobile_abis[@]}"; do wc -c "$MOBILE_ANDROID_OUT/app/src/main/jniLibs/$abi/liboink_mobile_ffi.so"; done
