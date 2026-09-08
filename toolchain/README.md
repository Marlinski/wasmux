# How `bin/` is produced

The `.wasm` images in `bin/` are checked in, so using wasmux needs nothing but Rust. The
compiled-in archive is *not* — its licences forbid distributing it (see
[THIRD-PARTY.md](../THIRD-PARTY.md)), so `build-archive.sh` is how you get the fast backend.
This directory is also here so the binaries can be audited against their sources, and so a
program can be added or an applet dropped.

```sh
./toolchain/fetch-tools.sh      # wabt and wasi-sdk, into toolchain/build/tools/
./toolchain/build-images.sh     # -> bin/*.wasm, bin/*.commands
./toolchain/build-archive.sh    # -> bin/libwasmux-images.a
```

Nothing outside `bin/` is written; `toolchain/build/` holds the sources and is not checked in.

## What is here

| | |
|---|---|
| `wcc` | the guest C compiler: clang targeting bare `wasm32`, against wasmux's musl |
| `musl/` | the wasm32 port, overlaid onto upstream musl |
| `busybox.config` | the BusyBox configuration, `CONFIG_NOMMU` and hush |
| `csrc/glue.c` | the seam between the wasm2c output and the Rust kernel |
| `csrc/sjlj-rt.c` | a `setjmp` runtime for the wasm exceptions lowering |
| `build-images.sh` | sources to `bin/*.wasm` |
| `build-archive.sh` | `bin/*.wasm` to `bin/libwasmux-images.a` |

## The musl port

A guest program is a Linux binary that happens to be a wasm module, so its libc is musl and
not WASI's. The port is small because almost all of musl is portable C; what it replaces is
the handful of files that assume hardware musl cannot have here.

| | |
|---|---|
| `arch-wasm32/` | the architecture: type sizes, `syscall_arch.h` forwarding to the `wasmux` import, a `jmp_buf` large enough for the kernel's saved stack pointer |
| `crt-wasm32/crt1.c` | startup. Calls the `args` import for the startup block rather than reading one the loader left, because there is no loader |
| `src/process/wasm32/` | `vfork` as a `setjmp` at the call site, and `execve` |
| `src/setjmp/wasm32/` | `setjmp` and `longjmp` as imports, which is what lets Asyncify implement them |
| `src/signal/wasm32/` | signal delivery through the `sigfetch` import and a flag musl polls |
| `src/thread/wasm32/` | `clone` returning `ENOSYS`, and a static thread pointer |
| `src/exit/wasm32/` | `_Exit` as a syscall, since the process cannot just stop |

The empty `siglongjmp.c` and `sigsetjmp.c` are deliberate. Upstream defines them through a
macro that, with `longjmp` as an import, makes `siglongjmp` call itself; an empty override
lets the generic definitions win. Removing them produces a 20000-frame stack overflow the
first time a shell handles a signal, which is a long way from the cause.

## The BusyBox configuration

Two choices matter:

- **`CONFIG_NOMMU`.** wasm has no MMU, so there is no `fork`, only `vfork` plus `exec`.
  BusyBox's `ash` refuses to build under `CONFIG_NOMMU`; hush is written for it. That is the
  whole reason the shell is hush.
- **No networking applets.** `wget`, `nc`, `ping`, `telnet` and the rest are off, because no
  socket syscall exists. Leaving them in would give an agent tools that always fail.

Changing the applet list is the only way to change what a sandbox can run: BusyBox dispatches
its own applets internally, so `Builder::programs` controls the namespace and not the
capability.

## Asyncify

Each program is instrumented by `wasm-opt --asyncify` with an explicit list of imports that
may unwind the guest's stack:

```
wasmux.setjmp, wasmux.longjmp, wasmux.syscall
```

The list lives in `build-images.sh` and nowhere else. `syscall` is on it because a blocking
syscall suspends the process; `setjmp` and `longjmp` because that is how they are implemented
at all. An import that can unwind but is missing from the list corrupts the guest's stack in a
way that surfaces much later, so treat the list as part of the ABI.

Instrumentation roughly doubles each module. That is the single largest cost in the design,
and `docs/DESIGN.md` explains what it buys.

## Rebuilding the archive for another target

`bin/libwasmux-images.a` is compiled for one target, recorded in `bin/MANIFEST.toml`. On a
different target the `aot` feature finds no archive, `build.rs` says so, and the interpreter
runs the same programs instead: slower, never broken. To get the fast path there:

```sh
TARGET=wasm32-wasip1 ./toolchain/build-archive.sh
```

The call-depth fuse is set with `WASMUX_MAX_DEPTH` (default 400). It is what turns a runaway
guest recursion into one dead process rather than a stack overflow of the whole module, so it
has to fit inside the host's wasm stack. Raising it means raising both together.
