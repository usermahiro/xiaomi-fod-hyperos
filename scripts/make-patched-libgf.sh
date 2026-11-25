#!/bin/bash
set -euo pipefail
SERIAL="${1:-}"
DELAY="${2:-30000}"
ADB=(adb)
[ -n "$SERIAL" ] && ADB+=( -s "$SERIAL" )
DIR="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

"${ADB[@]}" pull /odm/lib64/libgf_hal.so "$TMP/libgf_hal.so"
python3 "$DIR/gf-fingerprint-wrapper/patch_libgf_hal.py" \
    "$TMP/libgf_hal.so" -o "$TMP/libgf_hal_patched.so" --delay-us "$DELAY"
"${ADB[@]}" push "$TMP/libgf_hal_patched.so" /data/local/tmp/libgf_hal_patched.so
echo "OK: /data/local/tmp/libgf_hal_patched.so (delay=${DELAY}us)"
