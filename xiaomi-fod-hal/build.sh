#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROFILE="${1:-release}"

NDK="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-${ANDROID_NDK_LATEST_HOME:-}}}"
if [ -z "$NDK" ] || [ ! -d "$NDK" ]; then
    echo "ERROR: Android NDK not found. Export ANDROID_NDK_HOME=/path/to/ndk" >&2
    exit 1
fi

NDK_BIN="$(find "$NDK/toolchains/llvm/prebuilt" -maxdepth 2 -type d -name bin 2>/dev/null | head -n1)"
if [ -z "$NDK_BIN" ]; then
    echo "ERROR: LLVM prebuilt bin/ not found under $NDK" >&2
    exit 1
fi

API_LEVEL="${API_LEVEL:-33}"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK_BIN/aarch64-linux-android${API_LEVEL}-clang"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_AR="$NDK_BIN/llvm-ar"
export RUSTFLAGS="${RUSTFLAGS:--C link-arg=-Wl,--gc-sections}"

cd "$SCRIPT_DIR"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
if [ "$PROFILE" = "release" ]; then
    cargo build --target aarch64-linux-android --release
    BIN="$TARGET_DIR/aarch64-linux-android/release/xiaomi_fod_hal"
else
    cargo build --target aarch64-linux-android
    BIN="$TARGET_DIR/aarch64-linux-android/debug/xiaomi_fod_hal"
fi

if [ -x "$NDK_BIN/llvm-strip" ]; then
    "$NDK_BIN/llvm-strip" "$BIN" || true
fi
echo "OK: $BIN"
