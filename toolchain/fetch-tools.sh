#!/usr/bin/env bash
# Downloads the two binary toolchains build-archive.sh needs, into toolchain/build/tools/.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TOOLS=$HERE/build/tools
mkdir -p "$TOOLS"

WABT_VERSION=${WABT_VERSION:-1.0.41}
WASI_SDK_VERSION=${WASI_SDK_VERSION:-34.0}

# Both projects name their assets by architecture, and differently from each other: wabt says
# `linux-x64` / `linux-arm64`, wasi-sdk says `x86_64-linux` / `arm64-linux`.
case "$(uname -m)" in
  x86_64|amd64) WABT_ARCH=linux-x64;    SDK_ARCH=x86_64-linux ;;
  aarch64|arm64) WABT_ARCH=linux-arm64; SDK_ARCH=arm64-linux ;;
  *) echo "no prebuilt wabt/wasi-sdk for $(uname -m); build them yourself and set WABT and WASI_SDK" >&2; exit 2 ;;
esac

if [ ! -d "$TOOLS/wabt-$WABT_VERSION" ]; then
  echo "wabt $WABT_VERSION ($WABT_ARCH)"
  curl -fsSL "https://github.com/WebAssembly/wabt/releases/download/$WABT_VERSION/wabt-$WABT_VERSION-$WABT_ARCH.tar.gz" \
    | tar -xz -C "$TOOLS"
fi

if [ ! -d "$TOOLS/wasi-sdk-$WASI_SDK_VERSION-$SDK_ARCH" ]; then
  echo "wasi-sdk $WASI_SDK_VERSION ($SDK_ARCH)"
  major=${WASI_SDK_VERSION%%.*}
  curl -fsSL "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-$major/wasi-sdk-$WASI_SDK_VERSION-$SDK_ARCH.tar.gz" \
    | tar -xz -C "$TOOLS"
fi

# The release tarball puts the wasm2c runtime under `share/wabt/wasm2c` and its headers in
# `include`, where a source checkout has both under `wasm2c/`. `build-archive.sh` handles
# either; this is just where they landed.
ls -d "$TOOLS"/*
