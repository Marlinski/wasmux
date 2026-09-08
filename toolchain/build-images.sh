#!/usr/bin/env bash
# Produces bin/*.wasm and bin/*.commands from upstream sources.
#
# The guest programs are ordinary C, built for bare `wasm32` against wasmux's musl, whose
# syscall layer is a single imported function. Nothing here is WASI.
#
# Five steps:
#   1. fetch musl, BusyBox and jq
#   2. overlay musl/ onto musl and build it into a sysroot
#   3. build BusyBox with hush, and jq
#   4. run Binaryen's Asyncify over each program, which is what makes a process suspendable
#   5. write bin/<name>.wasm and bin/<name>.commands
#
# Needs: clang 20 with the wasm32 target, llvm-ar/nm/ranlib/strip 20, wasm-opt (Binaryen),
# curl, tar, make. Everything is built under toolchain/build/, which is not checked in.
#
#   ./toolchain/build-images.sh            # all of it, incrementally
#   ./toolchain/build-images.sh musl       # just one stage
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
WORK=$HERE/build
OUT=$ROOT/bin
JOBS=$(nproc 2>/dev/null || echo 4)

MUSL_VERSION=1.2.4
BUSYBOX_VERSION=1.37.0
JQ_VERSION=1.8.2
# Bundled in jq's release tarball under vendor/, and built separately here — see stage_jq.
ONIGURUMA_VERSION=6.9.10

export CLANG=${CLANG:-clang-20}
export WASMUX_SYSROOT=$WORK/sysroot
LLVM_SUFFIX=${LLVM_SUFFIX:--20}

# The tools are passed to configure by *name*, not by path, and `toolchain/` goes on PATH to
# resolve them. That is not tidiness: autotools records the whole configure line in the
# binary — `jq --build-configuration` prints it — so an absolute `CC=/home/you/...` ends up
# inside a file this project redistributes. Names keep the artifact free of anybody's
# directory layout, which also makes two people's builds compare.
export PATH="$HERE:$PATH"
CC=wcc
AR=$(basename "$(command -v "llvm-ar$LLVM_SUFFIX" || command -v llvm-ar)")
NM=$(basename "$(command -v "llvm-nm$LLVM_SUFFIX" || command -v llvm-nm)")
RANLIB=$(basename "$(command -v "llvm-ranlib$LLVM_SUFFIX" || command -v llvm-ranlib)")
STRIP=$(command -v "llvm-strip$LLVM_SUFFIX" || command -v llvm-strip)

# Asyncify has to know which imports may unwind the guest's stack. `syscall` is on the list
# because a blocking syscall suspends the process; `setjmp` and `longjmp` because that is how
# they are implemented at all. Getting this list wrong produces a program that hangs or
# corrupts its own stack, so it lives in exactly one place.
ASYNCIFY=(--asyncify --pass-arg=asyncify-imports@wasmux.setjmp,wasmux.longjmp,wasmux.syscall -O2)

# The shipped `configure` scripts are the ones we want. Autotools would rather regenerate
# them, and cannot: no libtool m4 is vendored. Pointing each tool at `true` turns every
# regeneration rule into a no-op.
NO_AUTOTOOLS=(AUTOCONF=true AUTOHEADER=true AUTOMAKE=true ACLOCAL=true)

say() { printf '\n== %s\n' "$*"; }

fetch() { # url sha256 -> tarball in $WORK/src
  local url=$1 dir=$WORK/src file=$dir/${1##*/}
  mkdir -p "$dir"
  [ -f "$file" ] || curl -fsSL "$url" -o "$file"
  echo "$file"
}

stage_musl() {
  say "musl $MUSL_VERSION"
  local src=$WORK/musl-$MUSL_VERSION
  if [ ! -d "$src" ]; then
    tar -xf "$(fetch https://musl.libc.org/releases/musl-$MUSL_VERSION.tar.gz)" -C "$WORK"
    # The port: a wasm32 architecture, a crt1 that calls the kernel's startup import, and
    # replacements for the handful of files that assume an MMU or a real signal frame.
    cp -r "$HERE/musl/arch-wasm32" "$src/arch/wasm32"
    cp -r "$HERE/musl/crt-wasm32" "$src/crt/wasm32"
    cp -r "$HERE/musl/src/." "$src/src/"
    # musl's configure has no idea what wasm32 is.
    grep -q 'wasm\*)' "$src/configure" ||
      sed -i 's|^x86_64-x32\*.*|wasm*) ARCH=wasm32 ;;\n&|' "$src/configure"
  fi
  cd "$src"
  [ -f config.mak ] || WCC_BARE=1 ./configure --target=wasm32 --prefix="$WASMUX_SYSROOT" \
      --disable-shared CC="$CC" AR="$AR" RANLIB="$RANLIB" CFLAGS="-O2" >/dev/null
  WCC_BARE=1 make -j"$JOBS" >/dev/null
  WCC_BARE=1 make install >/dev/null
  # BusyBox wants the kernel's own uapi headers. wasmux implements the asm-generic ABI, which
  # is what the host's copies describe, so they can be used as they are.
  if [ ! -d "$WASMUX_SYSROOT/include/linux" ]; then
    cp -r /usr/include/linux "$WASMUX_SYSROOT/include/linux"
    cp -r /usr/include/asm-generic "$WASMUX_SYSROOT/include/asm-generic"
    cp -r /usr/include/asm-generic "$WASMUX_SYSROOT/include/asm"
  fi
  cd "$ROOT"
}

stage_busybox() {
  say "busybox $BUSYBOX_VERSION"
  local src=$WORK/busybox-$BUSYBOX_VERSION
  if [ ! -d "$src" ]; then
    tar -xf "$(fetch https://busybox.net/downloads/busybox-$BUSYBOX_VERSION.tar.bz2)" -C "$WORK"
    # CONFIG_NOMMU, hush rather than ash, no networking applets. See toolchain/README.md.
    cp "$HERE/busybox.config" "$src/.config"
  fi
  cd "$src"
  make -j"$JOBS" CC="$CC" AR="$AR" NM="$NM" HOSTCC=gcc SKIP_STRIP=y busybox_unstripped >/dev/null
  "$STRIP" -o busybox.stripped.wasm busybox_unstripped
  asyncify busybox.stripped.wasm "$OUT/busybox.wasm"
  # The applet table, so the kernel can synthesise /bin without a hand-maintained list.
  make CC="$CC" AR="$AR" NM="$NM" HOSTCC=gcc busybox.links >/dev/null 2>&1 || true
  commands_from_links busybox.links > "$OUT/busybox.commands"
  cd "$ROOT"
}

# jq's `test`, `match`, `sub`, `gsub`, `capture`, `scan` and `splits` are all oniguruma, and
# without it they do not merely behave differently — they refuse, at run time, with "jq was
# compiled without ONIGURUMA regex library". Those are core jq idioms, so it is built.
#
# Built on its own rather than through jq's `--with-oniguruma=builtin`, which runs the
# vendored tree's configure as a sub-configure and then tries to regenerate it: the tarball
# ships no libtool m4, so autoreconf fails. Pointing the autotools at `true` is what stops
# every Makefile in both trees from trying to rebuild a `configure` that is already correct.
stage_oniguruma() {
  say "oniguruma $ONIGURUMA_VERSION"
  local src=$WORK/jq-$JQ_VERSION/vendor/oniguruma
  [ -d "$src" ] || { echo "oniguruma is missing from jq's tarball" >&2; return 1; }
  cd "$src"
  # `--prefix` is relative for the same reason the tools are named rather than pathed: it
  # would otherwise be recorded in jq's own build configuration string.
  [ -f Makefile ] || ./configure --host=wasm32-unknown-linux --disable-shared --enable-static \
      --prefix="$(cd "$WORK" && pwd)/onig" \
      CC="$CC" AR="$AR" RANLIB="$RANLIB" CFLAGS="-O2" >/dev/null
  make -j"$JOBS" "${NO_AUTOTOOLS[@]}" >/dev/null
  make install "${NO_AUTOTOOLS[@]}" >/dev/null
  cd "$ROOT"
}

stage_jq() {
  say "jq $JQ_VERSION"
  local src=$WORK/jq-$JQ_VERSION
  if [ ! -d "$src" ]; then
    tar -xf "$(fetch https://github.com/jqlang/jq/releases/download/jq-$JQ_VERSION/jq-$JQ_VERSION.tar.gz)" -C "$WORK"
  fi
  stage_oniguruma
  cd "$src"
  # Relative, and `../..` because jq's tree sits at $WORK/jq-$JQ_VERSION: it is the string
  # that matters, not the resolution — see the note by the tool variables.
  [ -f Makefile ] || ./configure --host=wasm32-unknown-linux --disable-shared --enable-static \
      --with-oniguruma=../onig --disable-docs --disable-valgrind \
      --disable-maintainer-mode \
      CC="$CC" AR="$AR" RANLIB="$RANLIB" CFLAGS="-O2" >/dev/null
  # A full rebuild, because a stale object from an earlier configure silently keeps its old
  # `-DHAVE_LIBONIG` state and you get a jq whose regex functions refuse at run time while
  # every configure check said yes.
  make clean "${NO_AUTOTOOLS[@]}" >/dev/null 2>&1 || true
  make -j"$JOBS" "${NO_AUTOTOOLS[@]}" >/dev/null
  "$STRIP" -o jq.stripped.wasm jq
  asyncify jq.stripped.wasm "$OUT/jq.wasm"
  printf 'jq\t/usr/bin/jq\n' > "$OUT/jq.commands"
  cd "$ROOT"
}

# Instrument a program so the kernel can suspend it. This roughly doubles the module and is
# the single largest cost in the whole design; see docs/DESIGN.md.
asyncify() {
  local from=$1 to=$2
  mkdir -p "$(dirname "$to")"
  wasm-opt "${ASYNCIFY[@]}" "$from" -o "$to"
  printf '   %-24s %s bytes\n' "$(basename "$to")" "$(stat -c%s "$to")"
}

# BusyBox writes one absolute path per line. The kernel wants name and path.
commands_from_links() {
  sed 's:^/*::' "$1" | while read -r path; do
    [ -n "$path" ] && printf '%s\t/%s\n' "${path##*/}" "$path"
  done | sort -u
}

manifest() {
  say "manifest"
  cd "$OUT" && sha256sum ./*.wasm ./*.commands > SHA256SUMS && cd "$ROOT"
  printf '   wrote bin/SHA256SUMS\n'
  printf '   remember to update the toolchain versions in bin/MANIFEST.toml\n'
}

mkdir -p "$WORK" "$OUT"
case "${1:-all}" in
  musl) stage_musl ;;
  busybox) stage_musl; stage_busybox; manifest ;;
  jq) stage_musl; stage_jq; manifest ;;
  all) stage_musl; stage_busybox; stage_jq; manifest ;;
  *) echo "usage: $0 [all|musl|busybox|jq]" >&2; exit 2 ;;
esac
say "done. Now run toolchain/build-archive.sh to compile them in."
