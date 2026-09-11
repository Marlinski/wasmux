# The guest ABI

The contract between the kernel and a program that runs in it. Anything built for wasmux must
match this; anything that matches it will run.

Version **1** (`wasmux::ABI_VERSION`). It changes when the imports, the syscall convention or
the layouts below change, and the version is what a mismatch is reported against.

## The target

`wasm32`, position-independent, linked against wasmux's musl. Linux `asm-generic` syscall
numbering, 32-bit, `time64`: the same numbering as `riscv32` or `arm64` Linux, not `x86`.

The module must export `memory`, `_start`, `__stack_pointer`, and Binaryen's four Asyncify
controls (`asyncify_start_unwind`, `asyncify_stop_unwind`, `asyncify_start_rewind`,
`asyncify_stop_rewind`). It is produced by running Binaryen's Asyncify pass with
`wasmux.syscall`, `wasmux.setjmp` and `wasmux.longjmp` declared as the imports that unwind.

## The six imports

All from module `wasmux`.

| import | signature | meaning |
|---|---|---|
| `syscall` | `(i32 number, i32 args) -> i32` | `args` points at six 32-bit words. Returns the result, or a negated errno. |
| `init` | `(i32 flag)` | Tells the kernel where musl's "a signal is pending" flag lives. Called once at startup. |
| `args` | `(i32 buffer, i32 capacity) -> i32` | With `capacity` zero, returns the size of the startup block. Otherwise writes it at `buffer` and returns its size. |
| `sigfetch` | `(i32 out) -> i32` | Returns 1 and fills `out` when a handler should run, 0 when nothing is pending. |
| `setjmp` | `(i32 jmp_buf) -> i32` | Returns 0 the first time, and the `longjmp` value afterwards. |
| `longjmp` | `(i32 jmp_buf, i32 value)` | Does not return. |

`setjmp` and `longjmp` are imports rather than compiled code because the kernel implements
them by saving and restoring the guest's stack; see `docs/DESIGN.md`.

## The startup block

What `args` writes, at the address the guest asked for, in this order. Every pointer is
absolute, computed from that address.

```
u32          argc
u32 * argc   argv pointers
u32          0
u32 * n      envp pointers
u32          0
(u32, u32)*  auxv key/value pairs, ending with AT_NULL
16 bytes     what AT_RANDOM points at
bytes        the argument, environment and AT_EXECFN strings, NUL-terminated
```

The auxiliary vector carries `AT_PAGESZ`, `AT_CLKTCK`, `AT_UID`, `AT_EUID`, `AT_GID`,
`AT_EGID`, `AT_SECURE`, `AT_RANDOM`, `AT_EXECFN` and `AT_NULL`.

## Signal delivery

There is no asynchronous delivery: nothing interrupts a running guest. Instead the kernel
raises the flag `init` was told about, musl checks it on the way out of every syscall, and
calls `sigfetch` until it returns 0. Each call that returns 1 writes five words at `out`:

```
u32  signal number
u32  handler address
u32  flags
u32  si_code   (always 0)
u32  si_pid    (always 0)
```

A signal whose action is to terminate does not come back through `sigfetch` at all; the kernel
ends the process. A blocked syscall returns `EINTR` whenever a handler is going to run, even
with `SA_RESTART`, because the handler runs on the way out of the syscall and nothing here can
reissue it afterwards.

## Syscalls

About 130 are implemented; the rest return `ENOSYS`. Notable choices:

- `statx` is the stat call. `fstat` and `newfstatat` return `ENOSYS`, which is what musl on
  this target falls back from.
- `lseek` takes a 32-bit offset directly, not the five-argument `_llseek` form.
- `clone` accepts only the `vfork` shape: `CLONE_VFORK | CLONE_VM | SIGCHLD`. Threads are
  `ENOSYS`.
- Every socket call is `ENOSYS`. There is no network.
- `mmap` grows the guest's memory; a file mapping is read once, eagerly, because there is no
  fault to make it lazy. `munmap` succeeds and does nothing.
- `utimensat` does not store times, but does require the path to exist, because `touch` uses
  its failure to decide whether to create the file.
- `ioctl` answers the terminal requests — `TIOCGWINSZ`, `TCGETS`, and `TCSETS` with the two
  that differ from it only in when they take effect — on the standard streams, and only when
  the session was given a terminal. Without one they are all `ENOTTY`, which is what makes
  `isatty` say no. `struct termios` is the `asm-generic` layout, 36 bytes: four flag words,
  the line discipline and 19 control characters. musl's own struct is larger and reads back
  only what was filled, exactly as on a real kernel.

## Building a program for wasmux

The toolchain that produces `bin/` is in `toolchain/`. In outline: compile against wasmux's
musl with clang targeting `wasm32`, link position-independent, then run Binaryen's Asyncify
pass declaring the three unwinding imports. `make images` does it.

A program built for WASI will not run: it imports `wasi_snapshot_preview1`, not `wasmux`, and
fails to instantiate. That is deliberate. The ABI is the boundary, and the only programs that
can run are the ones built for it.
