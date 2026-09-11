# How wasmux is built, and why

This is the reasoning behind the shape of the code. It is written for whoever has to change
it, and for anyone deciding whether to trust it.

## The problem

An agent needs to run shell commands. Writing the tools yourself, a `grep` that takes some of
grep's flags, a `jq` that is not quite jq, produces a surface that looks right and behaves
differently, and the differences surface as an agent quietly getting wrong answers. Running
the real tools instead means running real processes, and the agent is already inside a
WebAssembly sandbox where none of the usual machinery exists.

Three things are not available, and everything below follows from them:

- **No processes.** A component cannot spawn or exec anything.
- **No MMU.** There is one flat linear memory, no page tables, no faults, so no `fork`.
- **No threads, and no way to block.** Blocking the component blocks the agent's event loop.

## Why not the obvious alternatives

**A CPU emulator** running a real Linux kernel, the way container2wasm does with Bochs or
TinyEMU, is the most faithful option and about a hundred times slower. It also cannot see the
agent's own filesystem, only a disk image.

**A separate sandbox process**, or a capability in the host runtime, is faster and cleanly
isolated, but it cannot mount the agent's workspace, which lives inside the agent. That
requirement is what rules it out: the shell has to see the same files the agent sees.

**Static linking of BusyBox into the agent** fails on the first pipeline. Two live processes
need two copies of the C runtime, its globals, its malloc arena, its stdio buffers, and one
linked copy has one of each. BusyBox is also not re-entrant.

What is left is to run the programs as separate wasm instances *inside* the component, and to
implement the Linux syscall interface in Rust. That is what this is.

## The syscall layer is not a kernel

It implements the Linux ABI: syscall numbers, struct layouts, errno values, wait statuses. It
does not implement an operating system. There is no scheduler with priorities, no virtual
memory, no VFS with caching, no drivers. Roughly 130 syscalls out of 310, chosen by what a
shell and its tools actually call. Everything underneath is delegated: files go to the `Vfs`
the caller provided, time comes from the host clock, and memory is the wasm instance's own.

The honest name is a Linux personality, the same category as WSL1's `lxcore` or gVisor's
Sentry, and about four thousand lines rather than four million.

## Suspending a process without threads

This is the central mechanism, and the one worth understanding before changing anything.

A guest calls into the kernel from deep inside its own call stack. When a syscall cannot be
answered, a read on an empty pipe, a `wait4` with no dead child, a `Vfs` that said `AGAIN`,
there is no thread to park and no way to return to the guest later from the same point.

Binaryen's **Asyncify** pass solves it. It rewrites the guest so it can save and restore its
own call stack through memory: at each call site, code to spill locals into a scratch area on
the way out, and to restore them and jump back to the right call on the way in. The kernel
drives it through four exported functions.

So a blocking syscall does this: record why, ask Asyncify to unwind, and return out of the
guest entirely. The scheduler runs someone else. When the syscall can finish, the stack is
rewound, the import is re-entered, and it returns the value as if it had blocked all along.

`setjmp`, `longjmp`, `exit` and `execve` ride the same machinery, which is why the code has
one suspension path rather than five. `vfork` in particular is a `setjmp` at the call site: the
child runs inside the parent's instance under the child's identity, and `execve` hands the
child to the scheduler and returns the pid to the parent through a `longjmp`.

Two details that cost debugging time and are easy to get wrong again:

- **Asyncify saves locals but not globals.** The shadow stack pointer is a global, so the
  kernel snapshots it alongside each `jmp_buf` and restores it on the way back.
- **The retry must be idempotent.** A blocked syscall is called again with the same arguments,
  so nothing may be consumed before the call can commit. `read` stages into a buffer and only
  touches guest memory once it has data.

## No panics, by construction

On `wasm32` there is no unwinder. `catch_unwind` cannot catch a panic; it goes straight to
`abort`, which takes down the whole component, agent included. This was verified rather than
assumed, on stable and with a custom-built standard library, and both abort.

So the kernel cannot rely on catching its own bugs. Instead: `unwrap`, `expect`, `panic`,
indexing and unchecked arithmetic are denied by lint in the library; every guest-derived value
goes through a checked accessor that returns `EFAULT` rather than trapping; and allocation
sizes derived from guest input are bounded before they are made.

## What each limit protects against

| limit | what it stops | what the guest sees |
|---|---|---|
| `memory`, `memory_per_process` | one command exhausting the agent's memory | `ENOMEM` from `brk`/`mmap` |
| `processes` | a fork bomb | `EAGAIN` from `clone` |
| `open_files` | descriptor exhaustion | `EMFILE` |
| `output` | filling the model's context with output | truncation, reported in `Output::dropped` |
| `wall_clock` | a runaway command | the session ends, at a syscall boundary |
| the backend's call-depth fuse | a deep recursion overflowing the host's stack | that process dies with `SIGSEGV` |

The last one is the least tidy. A guest that recurses deeply uses host stack in both backends,
and a host stack overflow would kill the component. Each backend therefore stops the guest
first, with a trap of its own, at a depth chosen to fit. If a program legitimately needs to go
deeper, both the fuse and the host's stack have to be raised together.

The gap: a program that makes no syscalls at all, a pure compute loop, is only stopped when
the wall clock is next checked. Closing that properly means instrumenting the guests with a
countdown at function entry, which the ABI has room for and which is not built yet.

## Why two backends

They exist for different jobs, and the corpus runs through both, so a divergence between them
is a test failure. `cargo test` covers the interpreter; `make test-wasm` builds the same case
table into a `wasm32-wasip2` component and runs it under wasmtime against the compiled-in
programs. Both pass all 60 cases, and issue the identical number of syscalls on every
benchmark workload, which is a stronger statement than the corpus alone: the two backends are
not merely both correct, they are doing the same work.

A build with neither backend is refused at compile time rather than at run time, because the
two ways to arrive there both have a one-line fix.

**Compiled in (`aot`).** `wasm2c` translates each program to C, and that C is compiled into the
consumer's own module. The programs become ordinary functions; one process is one instance
struct plus a heap-allocated memory. Roughly two to three times native, the remaining cost
being software bounds checks and Asyncify. The set of programs is fixed when the consumer is
built, which for an agent is a feature: nothing runs that was not shipped.

**Interpreted (`interp`).** wasmi runs the same images. Ten to twenty times slower, and needs
nothing but Rust, which is what makes `cargo test` work on any host with no C toolchain and no
wabt. It is also the fallback: a target with no prebuilt archive still runs.

Neither backend loads a program from the filesystem. A sandbox's programs come from the layout
in `bin/`, fixed when the consumer is built, so a file the guest can write is never a file the
guest can execute. `Backend::load` is the seam where that would change, and it is deliberately
not reachable from `execve`.

One wrinkle worth recording: wasmi 2.0 dispatches instructions by tail call, which only has
bounded stack use when the optimizer turns the calls into jumps. An unoptimized build
overflows the host stack after a few thousand guest instructions. The library therefore
selects wasmi's `portable-dispatch` backend, because a library must not depend on how its
consumer is compiled.

## Where the abstractions are, and why

- **`Vfs` is synchronous with an `AGAIN` escape.** A syscall happens inside a guest call stack
  that cannot await. Making the trait async would infect everything; making it purely
  synchronous would force an integrator to block. `AGAIN` reuses the suspension machinery that
  already exists for pipes: the process suspends, the caller is told to await, the call is
  retried. A synchronous filesystem never sees it.
- **`Instance` and `Suspend` are separate traits.** `Instance` is the scheduler's view, taken
  between calls into the guest. `Suspend` is the view from inside an import, where the backend
  holds its own calling context. Splitting them is what lets the syscall layer be one piece of
  generic code instead of one per backend.
- **Everything shared is an index into a slab.** Descriptions, pipes and process records live
  in kernel-owned slabs and are referred to by key, so sharing costs a reference count rather
  than an `Rc`, there is no interior mutability, and the whole structure stays `Send`.
- **The kernel owns the virtual namespace.** `/bin`, `/dev`, `/proc` and the programs are
  synthesized and never reach a mount, so an integrator implements storage and nothing else.
  A directory the kernel invents yields to a real one of the same name.

## A terminal, when there really is one

`isatty` answering no is a fact about the usual deployment, not a principle. Inside an agent
it is simply true: the streams are buffers. In front of a person it is false, and the cost of
saying it anyway is a shell with no prompt, no history and no line editing — everything
BusyBox's `FEATURE_EDITING` was compiled in to provide, sitting in the image unreachable.

So a session can be given a terminal, and two things gate it: the `tty` feature at compile
time and `Command::terminal` at run time. Both default to off, and a consumer that does
nothing gets precisely the behaviour above — which is why the first tests in `tests/tty.rs`
are the ones asserting that nothing changed.

The part worth understanding is who owns the settings. The guest does. It reads them with
`TCGETS`, writes them with `TCSETS`, and the kernel keeps them for the session. The host
follows: `Session::terminal_raw` reports whether the guest has cleared `ICANON`, and
`Session::terminal_signals` whether it still wants Ctrl-C to raise one. Both flip within a
single line — BusyBox's line editor takes raw mode to read the line and gives it back to run
the command — so a host reads them every time round its loop rather than once.

Getting that wrong is the failure to design against: if both ends echo, every keystroke
appears twice; if neither does, typing is invisible. The host mirrors exactly what the guest
changed — `ICANON`, `ECHO`, `ECHONL`, `ISIG`, `VMIN`/`VTIME` — and nothing else. `cfmakeraw`
is the wrong tool here, because it also clears `OPOST`, which the guest did not ask for, and
the result is output that stairsteps down the screen.

What is still missing: Ctrl-C cannot interrupt a running command. While one runs, the guest
has restored `ISIG`, so the host's terminal raises `SIGINT` against the host process, and
there is no way into a session for a signal from outside. `Limits::wall_clock` is what bounds
a runaway command today. Delivering a signal into a session is the missing piece, and
`send_signal` in the syscall layer is where it would land.

## What is deliberately not here

- **Networking.** No socket syscall, no networking tool. Adding it would mean routing through
  the host's own HTTP client, which is the consumer's decision to make, not this library's.
- **Threads.** `clone` for threads returns `ENOSYS`. Every program here is single-threaded.
- **`fork`.** Needs copy-on-write, which needs an MMU. Possible in principle by copying a
  whole memory and rebuilding the stack with Asyncify; not needed by anything shipped.
- **A terminal, unless one is asked for.** By default `isatty` says no, truthfully: the
  standard streams are buffers the host owns, which keeps `jq` from emitting colour and
  shells out of line-editing paths. That is the right answer for an agent, and it stays the
  default. The `tty` feature and `Command::terminal` together change it for the case where
  the answer is different — a person at a keyboard. See *A terminal, when there really is
  one* above.
