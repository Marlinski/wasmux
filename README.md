# wasmux

A POSIX process sandbox that runs **inside your own WebAssembly component**.

wasmux gives a program real Linux processes: BusyBox's shell and tools, `jq`, anything else
built for it, with pipes, signals, `vfork`, `execve` and exit statuses. There is no CPU
emulator and no separate host process. The programs execute inside the module that embeds the
library, and the filesystem they see is one you supply.

```rust
use wasmux::{MemVfs, Sandbox};

let files = MemVfs::new().with_file("/data.json", br#"{"items":[1,2,3]}"#);
let sandbox = Sandbox::builder().mount("/", files).build()?;

let out = sandbox.shell("jq '.items | add' /data.json").output()?;
assert_eq!(out.stdout_string().trim(), "6");
```

It exists to give an AI agent a shell tool that is the real thing rather than an
approximation, without giving the agent, or a bug in a tool, any way out of the sandbox the
agent itself is already in.

## What you get

- **Real programs.** BusyBox 1.37 with 135 applets, and `jq` 1.8. Real `sed`, `awk`, `find`,
  `sort`, `tar`, `grep`, with their real flags and their real edge cases.
- **A real shell.** hush: pipelines, redirection, `$(...)`, functions, loops, traps, job
  control, `&&`, here-documents. 60 behaviour cases in the corpus say so.
- **Your filesystem.** Implement three methods of [`Vfs`] and the guest sees your storage.
  Mount several, mark any of them read-only, and the guest cannot name anything else.
- **Your own commands.** A `HostCommand` is Rust that appears in the guest namespace as an
  ordinary executable — piped, redirected, captured by `$( )`, with a real exit status. It is
  how you add a tool that cannot be a wasm guest: one that needs a network, or one that *is*
  a Rust crate.
- **No network, at all.** No socket syscall is implemented and no networking tool ships.
- **Nothing that can take the host down.** Every limit surfaces to the guest as an ordinary
  error. A trap kills one process, not the sandbox. The kernel is panic-free by construction,
  because on `wasm32` a panic would abort the whole component.

## What it costs

Measured on this machine, whole commands timed end to end, because that is the number an
integrator has to plan around. The compiled-in column is the component running under wasmtime;
the interpreted one is a native build.

| workload | compiled in | interpreted |
|---|---|---|
| start a process and exit | 0.2 ms | 3.5 ms |
| spawn 20 processes | 3.2 ms | 15 ms |
| four-stage pipeline over 2000 lines | 6.3 ms | 101 ms |
| `sed` over 5000 lines | 15 ms | 282 ms |
| `jq` summing 2000 numbers | 14 ms | 170 ms |
| write and read 1 MiB | 13 ms | 140 ms |
| all nine workloads | 105 ms | 1303 ms |

Both backends issue the identical number of syscalls on every workload, which is a useful
check that they are running the same programs the same way.

Each live process holds its own linear memory, a couple of megabytes for a shell, returned when
it exits.

```sh
cargo run --release -p wasmux-cli --bin wasmux-bench      # interpreted, native
make bench-wasm                                           # compiled in, under wasmtime
```

## Getting started

```sh
cargo test                                   # 40 tests, incl. 60 shell cases and 10 isolation checks
make test-wasm                               # the same 60 cases, compiled in, under wasmtime
cargo run -p wasmux-cli -- -c 'seq 1 5 | paste -sd, -'
cargo run -p wasmux-cli -- -d ./some/dir -c 'find . -name "*.rs" | head'
cargo run -p wasmux-cli -- --programs        # what is available
```

To use it in your own program, see **[docs/INTEGRATION.md](docs/INTEGRATION.md)**. It has a
worked `Vfs` over an asynchronous store and the loop to drive a command from an async handler.

## How it works

Three layers, and each one is small:

1. **The programs** are ordinary C, compiled to `wasm32` against a musl whose syscalls are one
   imported function. They are Linux binaries in everything but file format.
2. **The kernel** is this library: a process table, descriptors, pipes, signals, path
   resolution and about 130 syscalls, in dependency-free Rust. It answers what the programs
   ask for, in terms of the `Vfs` you provided.
3. **The backend** runs the programs. Either they are compiled into your module ahead of
   time by `wasm2c`, or they are interpreted by `wasmi`. The compiled-in one is about ten
   times faster and needs a prebuilt archive for your target; the interpreter needs nothing
   but Rust. Selected automatically, and they behave identically: the corpus runs through
   both.

There are no threads. A process that cannot proceed has its stack saved by Binaryen's Asyncify
pass and is resumed later, which is also how `setjmp`, `longjmp`, `exit` and `execve` work.
`docs/DESIGN.md` explains why, and what it costs.

## Layout

```
crates/wasmux/        the library
crates/wasmux-cli/    a terminal front end, and the benchmark
bin/                  the prebuilt guest programs and their command tables
docs/INTEGRATION.md   how to add this to your program
docs/DESIGN.md        why it is built this way
docs/ABI.md           the contract between the kernel and a guest program
toolchain/            how the contents of bin/ are produced
```

## Limits worth knowing

- **nommu.** There is no `fork`, only `vfork` plus `exec`, because wasm has no MMU. hush is
  built for that; `bash` and `ash` are not, which is why the shell is hush.
- **`Builder::programs` is not a security boundary.** It controls the namespace. BusyBox
  dispatches its own applets internally, so removing `sed` from the list does not stop
  `sh -c 'sed ...'`. Build an image with fewer applets to decide that.
- **The guest cannot execute a file it wrote.** Programs come from `bin/`, fixed when you
  build. Downloading a wasm module into the filesystem and running it does not work, by
  design.
- **No threads.** `clone` for threads returns `ENOSYS`.
- **No sockets, and a `HostCommand` is where a network belongs.** Reaching the outside is the
  embedder's decision, made under the embedder's policy, in the embedder's code.
- **A compute loop is not preemptible.** `Limits::wall_clock` bounds it at syscall boundaries,
  and the caller's own budget bounds each `step`; a program that makes no syscalls at all runs
  until the wall clock is checked.

## License

wasmux's own code is **MIT** — see `LICENSE`.

The guest programs in `bin/` are other people's software and keep their own licences:
`busybox.wasm` is **GPL-2.0-only**, `jq.wasm` is MIT with oniguruma (BSD-2-Clause) and
decNumber (ICU). **[THIRD-PARTY.md](THIRD-PARTY.md)** has the details, the source obligation
that comes with BusyBox, and the reason the compiled-in archive is built rather than shipped.
