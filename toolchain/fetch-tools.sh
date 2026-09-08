#!/usr/bin/env bash
# Downloads the two binary toolchains build-archive.sh needs, into toolchain/build/tools/.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TOOLS=$HERE/build/tools
mkdir -p "$TOOLS"

WABT_VERSION=${WABT_VERSION:-1.0.41}
WASI_SDK_VERSION=${WASI_SDK_VERSION:-34.0}

if [ ! -d "$TOOLS/wabt-$WABT_VERSION" ]; then
  echo "wabt $WABT_VERSION"
  curl -fsSL "https://github.com/WebAssembly/wabt/releases/download/$WABT_VERSION/wabt-$WABT_VERSION-ubuntu-20.04.tar.gz" \
    | tar -xz -C "$TOOLS"
fi

if [ ! -d "$TOOLS/wasi-sdk-$WASI_SDK_VERSION-x86_64-linux" ]; then
  echo "wasi-sdk $WASI_SDK_VERSION"
  major=${WASI_SDK_VERSION%%.*}
  curl -fsSL "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-$major/wasi-sdk-$WASI_SDK_VERSION-x86_64-linux.tar.gz" \
    | tar -xz -C "$TOOLS"
fi

# wabt ships the wasm2c runtime sources next to the binaries in the release tarball.
ls -d "$TOOLS"/*
