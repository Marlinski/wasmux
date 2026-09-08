# Putting wasmux in an agent

This is the guide for adding wasmux to a program that already exists — typically an agent
replacing a hand-written shell tool with a real one. It assumes you have read the crate
documentation's front page; everything here is about the join between the two, not about
wasmux itself.

The short version: implement `Vfs` over your storage, build one `Sandbox`, and run commands
through it. The rest of this document is the detail that makes those three steps go smoothly.

---

## 1. Depend on it

```toml
[dependencies]
wasmux = { git = "https://github.com/Marlinski/wasmux" }
```

The defaults are right for an agent, and the reason is worth a paragraph because it is not
obvious.

| feature | what it adds | when |
|---|---|---|
| `aot` (default) | the programs compiled into your module by wasm2c | always |
| `interp` (default) | wasmi, which runs the same programs by interpreting them | always |
| `host-vfs` | a `Vfs` over `std::fs` | native tools and tests only |
| `corpus` | the behaviour corpus and its runner | only if you want to run it yourself |

The backend is chosen at compile time, not at run time. On `wasm32` with `aot` and the
prebuilt archive present, `default_backend` returns the compiled-in one and the interpreter
branch is unreachable, so link-time optimization removes wasmi and the embedded `.wasm` images
outright. Measured on the CLI built for `wasm32-wasip2`:

| features | component |
|---|---|
| `aot` | 6,826,036 bytes |
| `aot`, `interp` | 6,826,134 bytes |
| `interp` | 3,596,620 bytes |

Ninety-eight bytes. So keeping `interp` on is free when the archive is there, and it is what
saves you when it is not.

**The archive is not shipped, and you build it to get the fast path.** It combines
GPL-2.0-only BusyBox-derived code with Apache-2.0 `wasm-rt` in one binary, and those two
licences are incompatible, so this project cannot hand you that file — see
[THIRD-PARTY.md](../THIRD-PARTY.md). Building it yourself is a one-off, takes about 45
seconds, and redistributes nothing:

```sh
git clone https://github.com/Marlinski/wasmux && cd wasmux
./toolchain/fetch-tools.sh                          # wabt and a wasi-sdk, into toolchain/build/
./toolchain/build-archive.sh                        # -> bin/libwasmux-images.a
TARGET=wasm32-wasip1 ./toolchain/build-archive.sh   # or for another target
```

Then tell your build where it went. A git dependency's checkout is not somewhere you can drop
a file, so `build.rs` reads an environment variable:

```sh
WASMUX_ARCHIVE_DIR=/path/to/wasmux/bin cargo build --target wasm32-wasip2
```

Set it in `.cargo/config.toml` (`[env]`), your `Makefile`, or CI — wherever your build already
keeps such things. `build.rs` re-runs when it changes, and says so if the directory has no
archive in it.

Until you do, `build.rs` prints a warning and `interp` runs the same programs — correct, about
ten times slower on compute. That is also what happens on a target the archive was not built
for. With `interp` *off* and no archive the crate does not compile at all: it refuses a
configuration with no usable backend, and the error says which of the two fixes you want.

**One thing to weigh before you do this for something you ship.** The incompatibility is about
*distribution*, not about building: a combined binary you build and run yourself conveys
nothing to anybody. If you distribute a component that links the archive, the same
GPL-2.0/Apache-2.0 problem applies to that component. Either stay on the interpreter for
shipped builds, or build an image set whose licences are Apache-compatible — `bin/` is
produced by `toolchain/build-images.sh` and nothing forces it to be BusyBox.

So: take the defaults, and build the archive when the speed matters and the licences allow.

## 2. Decide how each tool gets in

There are two ways a command can exist, and the choice is not about effort — it is about what
the command needs.

| | a wasm guest, in `bin/` | a [`HostCommand`], in your code |
|---|---|---|
| written in | C, built by `toolchain/build-images.sh` | Rust, in your crate |
| can do | pure computation over the `Vfs` | anything you can do |
| isolation | inside the sandbox, no host access | your code, your rules |
| speed | 2-3x native | native |
| reach for it when | the source exists and needs nothing but files | it needs a capability the sandbox does not have, or it *is* a Rust crate |

**Default to a guest.** BusyBox and `jq` are guests. Adding another C tool is a BusyBox config
change or one more entry in the toolchain script, and it costs the sandbox nothing.

**Two cases a guest cannot serve**, and both are real:

- **It needs the network.** There is no socket syscall in wasmux and there will not be: the
  isolation tests assert its absence, and that absence is most of what makes it safe to run
  model-authored commands here. A `curl` must therefore be implemented by whoever holds the
  HTTP client — you — under whatever policy you already apply to outbound requests.
- **Its value is a Rust crate.** A tool over `scraper`, `regex` or `serde_yaml` is not worth
  reimplementing in C to make it a guest.

A host command is a real process either way: it has a pid, holds descriptors, works in the
middle of a pipeline, is redirected by `>`, is captured by `$( )`, and its status reaches `$?`.
What it does not have is guest memory or syscalls. It is a function:

```rust
use wasmux::{Errno, Exit, HostCommand, Invocation, VfsResult};

struct Curl {
    http: MyHttpClient,
}

impl HostCommand for Curl {
    /// Only `-d @-` reads standard input, so say so from the arguments and the kernel
    /// drains it exactly when it should. Getting this wrong is the one sharp edge: a
    /// command that claims to read standard input, used as the first stage of a pipeline,
    /// waits for a standard input nothing will close.
    fn reads_stdin(&self, argv: &[Vec<u8>]) -> bool {
        argv.iter().any(|a| a == b"@-")
    }

    fn run(&self, call: &Invocation) -> VfsResult<Exit> {
        let args = call.args();
        // Started on the first call, answered on a later one: return `AGAIN` and the
        // process suspends while everything else keeps running. The retry is handed the
        // identical `Invocation`, so key your pending-request map on the arguments.
        match self.http.poll(&args) {
            None => {
                self.http.start(&args);
                Err(Errno::AGAIN)
            }
            Some(response) => Ok(Exit::from_stdout(response.body)),
        }
    }
}
```

Register it and it is in the namespace:

```rust
let sandbox = Sandbox::builder()
    .mount("/", workspace)
    .command("curl", Curl { http })      // -> /usr/bin/curl
    .command("yq", Yq)                   // -> /usr/bin/yq
    .build()?;
```

Registering a name a shipped image already uses replaces it, which is the supported way to
override a BusyBox applet with your own.

## 3. Implement `Vfs` over your storage

The full contract is on the trait. Three rules matter more than the rest:

- Paths arrive **absolute, normalized, and relative to the mount point**. No `..`, no empty
  components, no trailing slash. You can use them as keys directly; the kernel has already
  made escape impossible.
- Only `open`, `stat` and `readdir` are required. Everything else defaults to a read-only
  filesystem, so a first implementation is short.
- If your storage is asynchronous, return `Errno::AGAIN` and see section 5.

### The simple version: synchronous, one round trip per call

If your store can be read synchronously, or you are willing to block your own event loop
briefly, this is all it takes:

```rust
use wasmux::{DirEntry, Errno, FileType, OpenOptions, SeekFrom, Stat, Vfs, VfsFile, VfsResult};

struct WorkspaceVfs {
    workspace: std::sync::Arc<Workspace>,
}

impl Vfs for WorkspaceVfs {
    fn open(&self, path: &str, opts: &OpenOptions) -> VfsResult<Box<dyn VfsFile>> {
        let existing = match self.workspace.read_blocking(path) {
            Ok(bytes) => Some(bytes),
            Err(NotFound) => None,
            Err(e) => return Err(Errno::IO),
        };
        let data = match (existing, opts.create || opts.create_new) {
            (Some(_), true) if opts.create_new => return Err(Errno::EXIST),
            (Some(bytes), _) if !opts.truncate => bytes,
            (Some(_), _) => Vec::new(),
            (None, true) => Vec::new(),
            (None, false) => return Err(Errno::NOENT),
        };
        Ok(Box::new(WorkspaceFile {
            workspace: self.workspace.clone(),
            path: path.to_string(),
            data,
            pos: if opts.append { usize::MAX } else { 0 },
            dirty: false,
            writable: opts.write || opts.append,
        }))
    }

    fn stat(&self, path: &str, _follow: bool) -> VfsResult<Stat> {
        match self.workspace.entry_blocking(path) {
            Ok(entry) if entry.is_dir => Ok(Stat::dir()),
            Ok(entry) => Ok(Stat::file(entry.size)),
            Err(_) => Err(Errno::NOENT),
        }
    }

    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let listing = self.workspace.list_blocking(path).map_err(|_| Errno::NOENT)?;
        Ok(listing
            .into_iter()
            .map(|e| DirEntry::new(e.name, if e.is_dir { FileType::Dir } else { FileType::File }))
            .collect())
    }

    // Whole-file storage has no symlinks, and saying so saves the kernel a `stat` per path
    // component on every lookup.
    fn has_symlinks(&self) -> bool {
        false
    }

    fn mkdir(&self, _path: &str, _mode: u32) -> VfsResult<()> {
        Ok(()) // an object store has no directories to create
    }

    fn unlink(&self, path: &str) -> VfsResult<()> {
        self.workspace.remove_blocking(path).map_err(|_| Errno::NOENT)
    }
}
```

The file handle buffers, because a whole-file store has nothing finer:

```rust
struct WorkspaceFile { /* as above */ }

impl VfsFile for WorkspaceFile {
    fn read(&mut self, buf: &mut [u8]) -> VfsResult<usize> {
        let start = self.pos.min(self.data.len());
        let n = (self.data.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&self.data[start..start + n]);
        self.pos = start + n;
        Ok(n)
    }

    fn write(&mut self, bytes: &[u8]) -> VfsResult<usize> {
        if !self.writable { return Err(Errno::BADF); }
        let at = if self.pos == usize::MAX { self.data.len() } else { self.pos };
        if self.data.len() < at + bytes.len() { self.data.resize(at + bytes.len(), 0); }
        self.data[at..at + bytes.len()].copy_from_slice(bytes);
        self.pos = at + bytes.len();
        self.dirty = true;
        Ok(bytes.len())
    }

    fn seek(&mut self, from: SeekFrom) -> VfsResult<u64> { /* ... */ }

    fn stat(&self) -> VfsResult<Stat> { Ok(Stat::file(self.data.len() as u64)) }

    fn set_len(&mut self, len: u64) -> VfsResult<()> {
        self.data.resize(len as usize, 0);
        self.dirty = true;
        Ok(())
    }

    /// The write-back point. Called once, before the handle is dropped.
    fn close(&mut self) -> VfsResult<()> {
        if self.dirty {
            self.workspace.write_blocking(&self.path, &self.data).map_err(|_| Errno::IO)?;
            self.dirty = false;
        }
        Ok(())
    }
}
```

`close` is where a whole-file store commits, which is why the trait has it: `Drop` can neither
fail nor be awaited.

## 4. Build one sandbox, run many commands

A `Sandbox` is configuration. Build it once, when the agent starts, and clone it freely; every
command gets its own processes, memory and file handles.

```rust
use wasmux::{Limits, MemVfs, Sandbox};

let sandbox = Sandbox::builder()
    .mount("/work", WorkspaceVfs { workspace })   // the agent's files
    .mount("/tmp", MemVfs::new())                  // scratch that never touches storage
    .mount("/", MemVfs::new())                     // somewhere for the root to be
    .current_dir("/work")
    .limits(Limits {
        memory: 128 << 20,
        processes: 32,
        output: 256 << 10,          // what the model will see
        wall_clock: Some(std::time::Duration::from_secs(20)),
        ..Limits::default()
    })
    .build()?;
```

Mount `/tmp` as `MemVfs`: shell tools write intermediate files constantly, and none of that
should reach the agent's real storage or be visible afterwards.

## 5. Run a command from an async tool

`Command::output` runs to completion, which is right when nothing can block. From an async
handler with an asynchronous `Vfs`, drive the session instead. This is the loop to copy:

```rust
use wasmux::{Budget, Progress, Wait};

async fn run_shell(sandbox: &Sandbox, script: &str) -> Result<String, MyError> {
    let mut session = sandbox.shell(script).spawn()?;
    loop {
        match session.step(Budget::syscalls(50_000))? {
            Progress::Done(output) => {
                return Ok(format!(
                    "{}{}",
                    output.stdout_string(),
                    if output.status.success() { String::new() } else { format!("\nexit {}", output.status) }
                ));
            }
            // The budget ran out. Yielding here is what keeps an SSE stream alive while a
            // long pipeline runs.
            Progress::Yielded => yield_now().await,
            // The Vfs said AGAIN. Await whatever it started, then step again.
            Progress::Waiting(Wait::Vfs) => workspace.pending().await,
            Progress::Waiting(Wait::Stdin) => session.close_stdin(),
            Progress::Waiting(Wait::Until(delay)) => sleep(delay).await,
        }
    }
}
```

To stream output to a user as it appears, call `session.take_stdout()` inside the loop; it
returns only what is new.

### Making an asynchronous `Vfs` work

`Errno::AGAIN` means "not ready, ask me again". The kernel suspends only the process that
asked, reports `Wait::Vfs` to you, and calls the same method with the same arguments after you
step again. The implementation is a cache plus a pending set:

```rust
struct AsyncVfs {
    cache: Mutex<HashMap<String, Vec<u8>>>,
    pending: Mutex<HashMap<String, JoinHandle<Result<Vec<u8>, Error>>>>,
    workspace: Arc<Workspace>,
}

impl AsyncVfs {
    fn get(&self, path: &str) -> VfsResult<Vec<u8>> {
        if let Some(bytes) = self.cache.lock().unwrap().get(path) {
            return Ok(bytes.clone());
        }
        let mut pending = self.pending.lock().unwrap();
        match pending.entry(path.to_string()) {
            Occupied(entry) if entry.get().is_finished() => {
                let bytes = entry.remove().now_or_never()??;
                self.cache.lock().unwrap().insert(path.to_string(), bytes.clone());
                Ok(bytes)
            }
            Occupied(_) => Err(Errno::AGAIN),          // still in flight
            Vacant(slot) => {
                slot.insert(spawn(self.workspace.read(path.to_string())));
                Err(Errno::AGAIN)                       // started; come back
            }
        }
    }
}
```

Then `pending()` in the driver loop awaits whatever is in flight. Three rules keep this sound,
and they are on the trait: retries are identical, so caching by argument is safe; a write
either takes bytes or returns `AGAIN` having taken none; and `AGAIN` forever is a hang, so
return a real error when something cannot succeed.

Start with the synchronous version. Move to this one only if the blocking calls turn out to
hurt.

## 6. Describe the tool to the model

Generate the description from the sandbox rather than hardcoding it, so it cannot drift:

```rust
let commands: Vec<String> = sandbox.programs().into_iter().map(|p| p.name).collect();
let description = format!(
    "Run a POSIX shell command. Available commands: {}. \
     The working directory is /work, which is the agent's workspace. \
     /tmp is scratch space that is discarded. There is no network access.",
    commands.join(", ")
);
```

What changes for the model, compared with a hand-written tool: it gets real `sed`, `awk`,
`find`, `sort`, `tar` and `jq`, with their real flags and their real behaviour, and shell
syntax works, pipes, redirection, `$(...)`, loops, `&&`. What it loses is any tool the previous
implementation invented, and anything that needs the network.

## 7. What to check before shipping

- **Set `Limits`.** The defaults are generous. `wall_clock` and `output` are the two that
  matter for an agent, because they bound what a bad command costs and what reaches the model.
- **Mount `/tmp` as `MemVfs`.** Otherwise every temporary file lands in the workspace.
- **Decide the toolset at build time, not with `Builder::programs`.** That method controls the
  namespace, not what BusyBox's shell can dispatch to internally. To genuinely remove tools,
  build an image with fewer applets (`make images`).
- **Expect no network.** No socket syscall is implemented and no networking tool ships. If the
  agent needs to fetch something, it has to happen outside the sandbox.
- **Exit status.** `output.status.shell_code()` is what `$?` would be: the exit code, or 128
  plus the signal.

## 8. When something misbehaves

- `WASMUX_TRACE=1` prints one line per syscall to stderr, and one per suspension. It is the
  fastest way to see what a program is actually asking for.
- The `wasmux` CLI runs the same library over a host directory:
  `cargo run -p wasmux-cli -- -d ./some/dir -c 'your script here' --stats`.
- A `Deadlock` error means every process is blocked with nothing to wake it. If your `Vfs`
  returns `AGAIN` and the driver never awaits anything, that is the usual cause.
- `Error::WouldBlock` from `Command::output` means the `Vfs` returned `AGAIN`; use the session
  loop from section 5.
