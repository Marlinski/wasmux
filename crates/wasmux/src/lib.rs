//! A POSIX process sandbox that runs inside your own WebAssembly component.
//!
//! wasmux runs real programs, BusyBox's shell and tools, `jq`, anything else built for it, as
//! real Linux processes with pipes, signals, `vfork` and `execve`. There is no CPU emulator and
//! no separate host: the programs execute inside the module that embeds this library, and the
//! filesystem they see is one you supply by implementing [`Vfs`]. Nothing reaches the outside
//! world unless your `Vfs` lets it.
//!
//! ```no_run
//! use wasmux::{MemVfs, Sandbox};
//! # fn main() -> Result<(), wasmux::Error> {
//! let files = MemVfs::new().with_file("/data.json", br#"{"items":[1,2,3]}"#);
//! let sandbox = Sandbox::builder().mount("/", files).build()?;
//!
//! let out = sandbox.shell("jq '.items | add' /data.json").output()?;
//! assert_eq!(out.stdout_string().trim(), "6");
//! assert!(out.status.success());
//! # Ok(()) }
//! ```
//!
//! # Choosing an entry point
//!
//! [`Command::output`] runs a command to completion. It is the right call when your [`Vfs`] is
//! synchronous, which [`MemVfs`] and `HostVfs` are.
//!
//! [`Command::spawn`] gives you a [`Session`] you drive yourself with [`Session::step`]. Use it
//! when your filesystem is asynchronous, or when you must not block your own event loop: each
//! step does a bounded amount of work and hands control back. `docs/INTEGRATION.md` walks
//! through both, with a worked asynchronous `Vfs`.
//!
//! # What it costs
//!
//! Programs run at roughly two to three times native speed with the default engine, which
//! compiles them into your module ahead of time. Each live process holds its own linear
//! memory, a couple of megabytes for a shell, freed when it exits. [`Limits`] bounds all of
//! it, and every limit surfaces to the guest as an ordinary error rather than as a failure of
//! the host.
// Unit tests assert with `unwrap` and `panic!`. The denials below them exist because a panic
// in the library aborts the consumer's component; a panic in a test binary just fails a test.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

mod abi;
/// The behaviour corpus: a table of shell cases and a runner, shared by the host test suite
/// and the in-component runner so both backends are held to the same table.
#[cfg(feature = "corpus")]
pub mod corpus;
mod engine;
mod errno;
/// Commands the embedder implements in Rust, appearing in the guest namespace as ordinary
/// executables. The way to add a tool that cannot be a wasm guest.
pub mod host;
mod kernel;
mod nr;
mod programs;
mod slab;
/// The filesystem the guest sees. The page documents the contract an implementation has to
/// keep: how paths arrive, how to say "not yet", and what is never asked for.
pub mod vfs;

pub use engine::{LoadError, ABI_VERSION};
pub use errno::{Errno, VfsResult};
pub use host::{Exit, HostCommand, Invocation};
pub use kernel::{Budget, Limits};
pub use vfs::{DirEntry, EmptyVfs, FileType, MemVfs, OpenOptions, SeekFrom, Stat, Vfs, VfsFile};

#[cfg(feature = "host-vfs")]
pub use vfs::HostVfs;

use kernel::path::{Mount, Mounts, Overlay};
use kernel::{Idle, Kernel, ProgramEntry, Slice};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Something that stopped a sandbox from doing what was asked.
#[derive(Debug)]
pub enum Error {
    /// A program could not be found or loaded.
    Load(LoadError),
    /// No program of that name is in this sandbox. [`Sandbox::programs`] lists what is.
    NoSuchProgram(String),
    /// [`Command::output`] was used with a [`Vfs`] that asked to be called again. Drive the
    /// command with [`Command::spawn`] and [`Session::step`] instead.
    WouldBlock,
    /// The session's processes are all waiting for something that will never arrive.
    Deadlock,
    /// A mount point was not an absolute, normalized path.
    BadMountPoint(String),
    /// Nothing was mounted, and the program needs a filesystem.
    NoMounts,
    /// No program could be loaded, so the sandbox would have nothing to run.
    ///
    /// This means the build has no usable backend: either every backend feature is off, or
    /// `aot` is on for a target that `bin/libwasmux-images.a` was not built for and `interp`
    /// is off. Enabling `interp` is the fix in almost every case; see `docs/INTEGRATION.md`.
    NoPrograms,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Load(e) => write!(f, "{e}"),
            Error::NoSuchProgram(name) => write!(f, "no such program: {name}"),
            Error::WouldBlock => {
                write!(
                    f,
                    "the filesystem needs to be awaited; drive this command with Session::step"
                )
            }
            Error::Deadlock => write!(f, "every process is blocked with nothing to wait for"),
            Error::BadMountPoint(at) => write!(f, "mount point {at:?} must be an absolute path"),
            Error::NoMounts => write!(f, "nothing is mounted"),
            Error::NoPrograms => write!(
                f,
                "no programs are available: this build has no usable backend. \
                 Enable the `interp` feature, or build bin/libwasmux-images.a for this target."
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<LoadError> for Error {
    fn from(e: LoadError) -> Error {
        Error::Load(e)
    }
}

/// One program a sandbox can run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramInfo {
    /// The command name, as the guest would type it: `sh`, `jq`, `sed`.
    pub name: String,
    /// Where it appears in the guest's filesystem.
    pub path: String,
}

/// Every command compiled into this build, sorted by name.
///
/// Static data: no sandbox, no allocation of a kernel, nothing to configure. It is here
/// because a consumer usually needs the list somewhere a sandbox is not to hand — writing a
/// tool description, answering "is `sed` available?", telling a model what it can run — and
/// hardcoding a list that drifts from the shipped images is the failure worth designing out.
///
/// This is what *could* run. [`Sandbox::programs`] is what a particular sandbox will run,
/// which is narrower when [`Builder::programs`] was used and wider when a
/// [`HostCommand`] was registered.
///
/// ```
/// // BusyBox ships an `awk`, so this build can run one.
/// assert!(wasmux::commands().iter().any(|c| c.name == "awk"));
/// ```
pub fn commands() -> Vec<ProgramInfo> {
    let mut out: Vec<ProgramInfo> = programs::layout()
        .iter()
        .flat_map(|layout| layout.commands)
        .map(|command| ProgramInfo {
            name: command.name.to_string(),
            path: command.path.to_string(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

/// A configured sandbox: mounts, limits, and the programs that can run.
///
/// Cheap to clone and safe to share; each [`Command`] gets its own processes, filesystem
/// handles and memory. Build one per agent, not one per command.
#[derive(Clone)]
pub struct Sandbox {
    inner: Arc<Inner>,
}

struct Inner {
    mounts: Arc<Mounts>,
    overlay: Arc<Overlay>,
    programs: Arc<Vec<ProgramEntry>>,
    /// Command name to program table index, for `Sandbox::command`.
    by_name: BTreeMap<String, (usize, String)>,
    limits: Limits,
    env: Vec<(String, String)>,
    cwd: String,
}

impl Sandbox {
    /// Start configuring a sandbox.
    pub fn builder() -> Builder {
        Builder::new()
    }

    /// Every program this sandbox can run, sorted by name.
    ///
    /// Generate your tool's documentation from this rather than hardcoding a list: it is the
    /// truth about what was compiled in.
    pub fn programs(&self) -> Vec<ProgramInfo> {
        self.inner
            .by_name
            .iter()
            .map(|(name, (_, path))| ProgramInfo {
                name: name.clone(),
                path: path.clone(),
            })
            .collect()
    }

    /// Whether `name` is one of them.
    pub fn has_program(&self, name: &str) -> bool {
        self.inner.by_name.contains_key(name)
    }

    /// Prepare to run `program`, which may be a command name or an absolute path.
    pub fn command(&self, program: &str) -> Command<'_> {
        Command {
            sandbox: self,
            program: program.to_string(),
            args: Vec::new(),
            env: self.inner.env.clone(),
            cwd: self.inner.cwd.clone(),
            stdin: Vec::new(),
            stdin_closed: true,
            #[cfg(feature = "tty")]
            terminal: None,
        }
    }

    /// Prepare to run a shell script: `sh -c <script>`.
    pub fn shell(&self, script: &str) -> Command<'_> {
        let mut command = self.command("sh");
        command.args.push("-c".to_string());
        command.args.push(script.to_string());
        command
    }

    /// The limits every session of this sandbox runs under.
    pub fn limits(&self) -> Limits {
        self.inner.limits
    }
}

/// Configures a [`Sandbox`].
pub struct Builder {
    mounts: Vec<Mount>,
    limits: Limits,
    env: Vec<(String, String)>,
    cwd: String,
    programs: Option<Vec<String>>,
    commands: Vec<HostEntry>,
}

/// One registered [`HostCommand`] and where it appears.
struct HostEntry {
    name: String,
    path: String,
    command: Arc<dyn HostCommand>,
}

impl Default for Builder {
    fn default() -> Builder {
        Builder::new()
    }
}

impl Builder {
    /// A builder with the default limits, a default environment and every available program.
    pub fn new() -> Builder {
        Builder {
            mounts: Vec::new(),
            commands: Vec::new(),
            limits: Limits::default(),
            env: vec![
                (
                    "PATH".to_string(),
                    "/bin:/usr/bin:/sbin:/usr/sbin".to_string(),
                ),
                ("HOME".to_string(), "/".to_string()),
                ("TERM".to_string(), "dumb".to_string()),
                ("PS1".to_string(), "$ ".to_string()),
            ],
            cwd: "/".to_string(),
            programs: None,
        }
    }

    /// Mount a filesystem, writable, at `at`.
    ///
    /// `at` must be absolute and normalized: `/` or `/work`, not `work` or `/work/`. The
    /// longest matching mount wins, so mounting [`MemVfs`] at `/tmp` over a slow store at `/`
    /// gives scratch space that never touches it.
    pub fn mount(mut self, at: &str, vfs: impl Vfs) -> Builder {
        self.mounts.push(Mount {
            at: at.to_string(),
            vfs: Arc::new(vfs),
            read_only: false,
        });
        self
    }

    /// Mount a filesystem that refuses writes. The refusal is the kernel's, so the [`Vfs`]
    /// needs no logic of its own.
    pub fn mount_ro(mut self, at: &str, vfs: impl Vfs) -> Builder {
        self.mounts.push(Mount {
            at: at.to_string(),
            vfs: Arc::new(vfs),
            read_only: true,
        });
        self
    }

    /// Mount an already shared filesystem, so one `Vfs` can back several sandboxes.
    pub fn mount_arc(mut self, at: &str, vfs: Arc<dyn Vfs>, read_only: bool) -> Builder {
        self.mounts.push(Mount {
            at: at.to_string(),
            vfs,
            read_only,
        });
        self
    }

    /// Replace the resource limits.
    pub fn limits(mut self, limits: Limits) -> Builder {
        self.limits = limits;
        self
    }

    /// Set an environment variable for every command.
    pub fn env(mut self, key: &str, value: &str) -> Builder {
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    /// Remove every default environment variable.
    pub fn clear_env(mut self) -> Builder {
        self.env.clear();
        self
    }

    /// The working directory commands start in. Defaults to `/`.
    pub fn current_dir(mut self, path: &str) -> Builder {
        self.cwd = path.to_string();
        self
    }

    /// Offer only these programs, by command name.
    ///
    /// The default is everything available. What this controls is the *namespace*: a command
    /// left out has no path, so nothing can `exec` it and `test -e /bin/sed` says no.
    ///
    /// It is not a security boundary, and here is why. BusyBox is a single program with its
    /// tools inside it, and its shell runs most of them without an `exec` at all. Leave `sed`
    /// out of this list and `sh -c 'sed ...'` still works. To decide what a shell can actually
    /// do, build an image with fewer applets: `make images` in the wasmux checkout takes a
    /// BusyBox configuration, and that is the decision point.
    pub fn programs(mut self, names: &[&str]) -> Builder {
        self.programs = Some(names.iter().map(|n| (*n).to_string()).collect());
        self
    }

    /// Add a command implemented in Rust, at `/usr/bin/<name>`.
    ///
    /// This is how you add a tool that cannot be a wasm guest: one that needs a capability
    /// wasmux does not have (a network, most obviously), or one whose value is a Rust crate.
    /// Everything else should be a `.wasm` in `bin/` — see [`host`] for the choice.
    ///
    /// It is a program in every respect the guest can observe: it appears in `/usr/bin`, in
    /// [`Sandbox::programs`], and in `$PATH`; it can be piped into and out of, redirected,
    /// captured by `$( )`, and `exec`ed. Registering a name a shipped image already uses
    /// replaces it, which is the supported way to override one.
    ///
    /// ```no_run
    /// # use wasmux::{Exit, HostCommand, Invocation, MemVfs, Sandbox, VfsResult};
    /// # struct Curl;
    /// # impl HostCommand for Curl {
    /// #     fn run(&self, _: &Invocation) -> VfsResult<Exit> { Ok(Exit::default()) }
    /// # }
    /// let sandbox = Sandbox::builder()
    ///     .mount("/", MemVfs::new())
    ///     .command("curl", Curl)
    ///     .build()?;
    /// # Ok::<(), wasmux::Error>(())
    /// ```
    pub fn command(self, name: &str, command: impl HostCommand) -> Builder {
        let path = format!("/usr/bin/{name}");
        self.command_at(name, &path, command)
    }

    /// As [`Builder::command`], but you choose the path.
    ///
    /// Only needed when a tool must be found where convention puts it — `/bin/`, `/sbin/` —
    /// because a script hardcodes that path.
    pub fn command_at(mut self, name: &str, path: &str, command: impl HostCommand) -> Builder {
        self.commands.push(HostEntry {
            name: name.to_string(),
            path: path.to_string(),
            command: Arc::new(command),
        });
        self
    }

    /// Finish, or say why not.
    pub fn build(self) -> Result<Sandbox, Error> {
        for mount in &self.mounts {
            let normalized = kernel::path::join(&kernel::path::normalize("/", &mount.at));
            if mount.at != normalized {
                return Err(Error::BadMountPoint(mount.at.clone()));
            }
        }
        if self.mounts.is_empty() {
            return Err(Error::NoMounts);
        }

        let backend = engine::default_backend();
        let wanted = self.programs;
        let mut entries: Vec<ProgramEntry> = Vec::new();
        let mut by_name: BTreeMap<String, (usize, String)> = BTreeMap::new();
        let mut paths: BTreeMap<String, usize> = BTreeMap::new();

        for layout in programs::layout() {
            let Some(program) = backend.builtin(layout.image) else {
                continue;
            };
            let index = entries.len();
            entries.push(ProgramEntry {
                name: layout.image.to_string(),
                kind: kernel::ProgramKind::Guest(program),
            });
            for command in layout.commands {
                if let Some(allowed) = &wanted {
                    if !allowed.iter().any(|a| a == command.name) {
                        continue;
                    }
                }
                paths.insert(command.path.to_string(), index);
                by_name.insert(command.name.to_string(), (index, command.path.to_string()));
            }
        }

        for host in self.commands {
            let index = entries.len();
            entries.push(ProgramEntry {
                name: host.name.clone(),
                kind: kernel::ProgramKind::Host(host.command),
            });
            // Registered last on purpose: a host command of the same name as a BusyBox
            // applet replaces it, which is how you override one.
            paths.insert(host.path.clone(), index);
            by_name.insert(host.name, (index, host.path));
        }

        // A sandbox with no programs would build cleanly and then fail on every command with
        // `NoSuchProgram`, which sends whoever hits it looking in the wrong place. Fail here,
        // where the cause is still visible.
        if entries.is_empty() {
            return Err(Error::NoPrograms);
        }

        Ok(Sandbox {
            inner: Arc::new(Inner {
                mounts: Arc::new(Mounts::new(self.mounts)),
                overlay: Arc::new(Overlay::new(paths)),
                programs: Arc::new(entries),
                by_name,
                limits: self.limits,
                env: self.env,
                cwd: self.cwd,
            }),
        })
    }
}

/// A command about to run.
pub struct Command<'a> {
    sandbox: &'a Sandbox,
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: String,
    stdin: Vec<u8>,
    stdin_closed: bool,
    #[cfg(feature = "tty")]
    terminal: Option<(u16, u16)>,
}

impl Command<'_> {
    /// Add one argument.
    pub fn arg(mut self, value: impl Into<String>) -> Self {
        self.args.push(value.into());
        self
    }

    /// Add several arguments.
    pub fn args<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(values.into_iter().map(Into::into));
        self
    }

    /// Set an environment variable for this command only.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    /// Run in this directory instead of the sandbox's default.
    pub fn current_dir(mut self, path: impl Into<String>) -> Self {
        self.cwd = path.into();
        self
    }

    /// Feed these bytes as standard input, then end of file.
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = bytes.into();
        self.stdin_closed = true;
        self
    }

    /// Leave standard input open, to be fed with [`Session::write_stdin`].
    pub fn interactive_stdin(mut self) -> Self {
        self.stdin_closed = false;
        self
    }

    /// Tell the guest its standard streams are a terminal `columns` by `rows`.
    ///
    /// Off by default, and the default is what an agent wants: with no terminal `isatty` says
    /// no, which keeps `jq` from emitting colour codes and keeps a shell out of its line
    /// editor, so what comes back is text rather than a recording of a screen. Pass this only
    /// when a person is at a keyboard on the other end of the streams.
    ///
    /// With a terminal, a shell runs its interactive path: a real prompt, expanded from `PS1`.
    /// Line editing, history and completion need one thing more — the guest will ask for raw
    /// mode, and the host has to match it, or both ends echo every keystroke. Poll
    /// [`Session::terminal_raw`] each time round the loop and put your own terminal into raw
    /// mode when it says so.
    ///
    /// Requires the `tty` feature.
    #[cfg(feature = "tty")]
    pub fn terminal(mut self, columns: u16, rows: u16) -> Self {
        self.terminal = Some((columns, rows));
        self
    }

    /// Run to completion and collect the output.
    ///
    /// Requires a [`Vfs`] and any [`HostCommand`]s that never return [`Errno::AGAIN`];
    /// otherwise this reports [`Error::WouldBlock`] and you should use [`Command::spawn`].
    pub fn output(self) -> Result<Output, Error> {
        let mut session = self.spawn()?;
        loop {
            match session.step(Budget::unlimited())? {
                Progress::Done(output) => return Ok(output),
                Progress::Yielded => {}
                // Nothing here can await, so a request to come back later cannot be honoured.
                Progress::Waiting(Wait::Vfs | Wait::Host) => return Err(Error::WouldBlock),
                Progress::Waiting(Wait::Stdin) => session.close_stdin(),
                Progress::Waiting(Wait::Until(_)) => {}
            }
        }
    }

    /// Start the command without running it, so the caller drives it step by step.
    pub fn spawn(self) -> Result<Session, Error> {
        let inner = self.sandbox.inner.clone();
        let (index, path) = match inner.by_name.get(&self.program) {
            Some((index, path)) => (*index, path.clone()),
            None => {
                // An absolute path is allowed too, so a script can be run directly.
                let found = inner
                    .by_name
                    .values()
                    .find(|(_, path)| *path == self.program);
                match found {
                    Some((index, path)) => (*index, path.clone()),
                    None => return Err(Error::NoSuchProgram(self.program.clone())),
                }
            }
        };

        let mut kernel = Kernel::new(
            inner.mounts.clone(),
            inner.overlay.clone(),
            inner.programs.clone(),
            inner.limits,
        );
        let mut argv: Vec<Vec<u8>> = Vec::with_capacity(self.args.len().saturating_add(1));
        argv.push(self.program.clone().into_bytes());
        for arg in &self.args {
            argv.push(arg.clone().into_bytes());
        }
        let envp: Vec<Vec<u8>> = self
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}").into_bytes())
            .collect();
        #[cfg(feature = "tty")]
        if let Some((columns, rows)) = self.terminal {
            kernel.set_terminal(columns, rows);
        }
        kernel.spawn_first(index, path, argv, envp, self.cwd.clone())?;
        kernel.shared.stdin.data.extend(self.stdin.iter().copied());
        kernel.shared.stdin.closed = self.stdin_closed;
        Ok(Session {
            kernel,
            finished: None,
        })
    }
}

/// How far a [`Session::step`] got.
#[derive(Debug)]
pub enum Progress {
    /// Everything is finished.
    Done(Output),
    /// The budget ran out with work still to do. Call [`Session::step`] again; between calls
    /// is where an asynchronous caller lets its own event loop run.
    Yielded,
    /// Nothing can progress until the caller does something.
    Waiting(Wait),
}

/// What a waiting session needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// A [`Vfs`] returned [`Errno::AGAIN`]. Await whatever it started, then step again.
    Vfs,
    /// A [`HostCommand`] returned [`Errno::AGAIN`]. Await whatever it started, then step
    /// again. Handled identically to [`Wait::Vfs`] — the two are separate only so a driver
    /// can tell which of its own subsystems it is waiting on.
    Host,
    /// A process is reading standard input. Call [`Session::write_stdin`] or
    /// [`Session::close_stdin`].
    Stdin,
    /// Everyone is sleeping. Wait roughly this long, then step again.
    Until(Duration),
}

/// A finished command.
#[derive(Debug, Clone)]
pub struct Output {
    /// How it ended.
    pub status: ExitStatus,
    /// Everything it wrote to standard output.
    pub stdout: Vec<u8>,
    /// Everything it wrote to standard error.
    pub stderr: Vec<u8>,
    /// Bytes dropped because [`Limits::output`] was reached.
    pub dropped: u64,
}

impl Output {
    /// Standard output as text, with invalid sequences replaced.
    pub fn stdout_string(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }
    /// Standard error as text, with invalid sequences replaced.
    pub fn stderr_string(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }
    /// Whether any output was truncated.
    pub fn truncated(&self) -> bool {
        self.dropped > 0
    }
}

/// How a command ended: the wait status a shell would report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus(i32);

impl ExitStatus {
    /// Exited with status zero.
    pub fn success(self) -> bool {
        self.code() == Some(0)
    }
    /// The exit code, if it exited normally.
    pub fn code(self) -> Option<i32> {
        if self.0 & 0x7f == 0 {
            Some((self.0 >> 8) & 0xff)
        } else {
            None
        }
    }
    /// The signal that killed it, if one did.
    pub fn signal(self) -> Option<i32> {
        let signal = self.0 & 0x7f;
        if signal == 0 {
            None
        } else {
            Some(signal)
        }
    }
    /// What a shell would put in `$?`: the code, or 128 plus the signal.
    pub fn shell_code(self) -> i32 {
        match self.signal() {
            Some(signal) => 128i32.saturating_add(signal),
            None => self.code().unwrap_or(0),
        }
    }
}

impl core::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.signal() {
            Some(signal) => write!(f, "killed by signal {signal}"),
            None => write!(f, "exit status {}", self.code().unwrap_or(0)),
        }
    }
}

/// A running command, driven by the caller.
///
/// Each [`Session::step`] runs until the budget is spent or nothing can progress, so a caller
/// with its own event loop keeps control. Output can be read as it appears with
/// [`Session::take_stdout`], which is what a tool that streams to a user needs.
pub struct Session {
    kernel: Kernel,
    finished: Option<i32>,
}

impl Session {
    /// Run for a while.
    pub fn step(&mut self, budget: Budget) -> Result<Progress, Error> {
        if let Some(status) = self.finished {
            return Ok(Progress::Done(self.collect(status)));
        }
        match self.kernel.run(budget) {
            Slice::Finished(status) => {
                self.finished = Some(status);
                Ok(Progress::Done(self.collect(status)))
            }
            Slice::Budget | Slice::Progressed => Ok(Progress::Yielded),
            Slice::Idle(Idle::Vfs) => Ok(Progress::Waiting(Wait::Vfs)),
            Slice::Idle(Idle::Host) => Ok(Progress::Waiting(Wait::Host)),
            Slice::Idle(Idle::Stdin) => Ok(Progress::Waiting(Wait::Stdin)),
            Slice::Idle(Idle::Until(when)) => Ok(Progress::Waiting(Wait::Until(
                when.saturating_duration_since(std::time::Instant::now()),
            ))),
            Slice::Idle(Idle::Deadlock) => Err(Error::Deadlock),
        }
    }

    /// Feed standard input.
    pub fn write_stdin(&mut self, bytes: &[u8]) {
        self.kernel.shared.stdin.data.extend(bytes.iter().copied());
        self.kernel.shared.stdin.wanted = false;
    }

    /// Signal end of file on standard input.
    pub fn close_stdin(&mut self) {
        self.kernel.shared.stdin.closed = true;
        self.kernel.shared.stdin.wanted = false;
    }

    /// Standard output written since the last call.
    pub fn take_stdout(&mut self) -> Vec<u8> {
        self.kernel.shared.stdout.drain_new()
    }

    /// Standard error written since the last call.
    pub fn take_stderr(&mut self) -> Vec<u8> {
        self.kernel.shared.stderr.drain_new()
    }

    /// How many processes are alive.
    pub fn process_count(&self) -> usize {
        self.kernel.processes()
    }

    /// Syscalls served so far, which is the cheapest measure of work done.
    pub fn syscall_count(&self) -> u64 {
        self.kernel.shared.syscalls
    }

    /// Whether the guest has put its terminal into raw mode, and the host should match.
    ///
    /// Always false without [`Command::terminal`]. With it, this is how a shell says it has
    /// taken over line editing: it has cleared `ICANON`, so it wants a keystroke at a time
    /// and will echo and erase for itself. Until the host stops doing the same, every
    /// keystroke appears twice. It flips back while a command runs and again for the next
    /// line, so read it every time round the loop rather than once.
    ///
    /// Requires the `tty` feature.
    #[cfg(feature = "tty")]
    pub fn terminal_raw(&self) -> bool {
        self.kernel.terminal_raw()
    }

    /// Whether the guest still wants Ctrl-C and friends to raise signals.
    ///
    /// A shell clears this for the length of a line, because it handles Ctrl-C itself while
    /// editing, and restores it to run a command. Mirror it the same way as
    /// [`Session::terminal_raw`]: a host that keeps generating signals when the guest asked
    /// for the bytes will kill the wrong thing.
    ///
    /// Requires the `tty` feature.
    #[cfg(feature = "tty")]
    pub fn terminal_signals(&self) -> bool {
        self.kernel.terminal_signals()
    }

    fn collect(&mut self, status: i32) -> Output {
        Output {
            status: ExitStatus(status),
            stdout: core::mem::take(&mut self.kernel.shared.stdout.data),
            stderr: core::mem::take(&mut self.kernel.shared.stderr.data),
            dropped: self
                .kernel
                .shared
                .stdout
                .dropped
                .saturating_add(self.kernel.shared.stderr.dropped),
        }
    }
}
