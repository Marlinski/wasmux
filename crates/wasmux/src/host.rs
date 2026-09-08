//! Commands the embedder implements in Rust.
//!
//! A guest program is C compiled to `wasm32` and shipped in `bin/`. That covers anything
//! whose source you can build, and it is the right answer whenever it applies. Two kinds of
//! command it does not cover:
//!
//! * **Anything that needs a capability wasmux does not have.** `curl` is the example. There
//!   is no socket syscall here and there is not going to be: the isolation tests assert its
//!   absence, and that absence is most of what makes wasmux safe to put inside an agent. A
//!   `curl` therefore has to be implemented by whoever *does* hold a network — the embedder,
//!   under the embedder's own policy.
//! * **Anything whose value is a Rust crate.** A tool built on `scraper`, `regex` or
//!   `serde_yaml` is not worth reimplementing in C so it can be a guest.
//!
//! A [`HostCommand`] closes that gap. Register one on the [`Builder`](crate::Builder) and it
//! appears in the guest's namespace as an ordinary executable:
//!
//! ```no_run
//! use wasmux::{HostCommand, Invocation, Exit, MemVfs, Sandbox, VfsResult};
//!
//! struct Hostname;
//!
//! impl HostCommand for Hostname {
//!     fn run(&self, call: &Invocation) -> VfsResult<Exit> {
//!         let _ = call;
//!         Ok(Exit::from_stdout("agent-01\n"))
//!     }
//! }
//!
//! let sandbox = Sandbox::builder()
//!     .mount("/", MemVfs::new())
//!     .command("hostname", Hostname)
//!     .build()?;
//!
//! // It is a program like any other: pipelines, redirection, `$(...)`, `execve`.
//! let out = sandbox.shell("echo \"host: $(hostname)\" | tr a-z A-Z").output()?;
//! assert_eq!(out.stdout_string().trim(), "HOST: AGENT-01");
//! # Ok::<(), wasmux::Error>(())
//! ```
//!
//! # What it is, mechanically
//!
//! A real process. It has a pid, it appears in `ps`, it holds descriptors, it can be the
//! middle of a pipeline, `>` redirects it, `$( )` captures it, `kill` reaches it, and its
//! exit status propagates the way any other program's does. The kernel does its reading and
//! writing through the same descriptions the guests use, so none of that is special-cased.
//!
//! What it does not have is guest memory, an Asyncify stack, or the ability to make syscalls.
//! It is a function from an [`Invocation`] to an [`Exit`], which is why it is easy to write.
//!
//! # Blocking
//!
//! Same convention as [`Vfs`](crate::Vfs), because it is the same machinery: return
//! `Err(Errno::AGAIN)` to mean "started, ask me again". The process suspends, everything else
//! keeps running, and [`Session::step`](crate::Session::step) reports
//! [`Wait::Host`](crate::Wait::Host) so the caller can await its own future. The retry passes
//! the identical [`Invocation`], so an implementation must be idempotent and may cache by
//! argument — a `curl` starts the request on the first call and returns the response on a
//! later one.

use crate::errno::VfsResult;

/// What a [`HostCommand`] was asked to do.
///
/// Everything a program gets at startup, in the form Rust wants it rather than the form the
/// C ABI wants it. Arguments and the environment are `Vec<u8>` because a shell can produce a
/// path that is not UTF-8; [`Invocation::args`] is the convenient view for the common case.
pub struct Invocation {
    /// The full argument vector. `argv[0]` is the name the command was invoked as, so a
    /// single implementation can serve several names.
    pub argv: Vec<Vec<u8>>,
    /// The environment, in `KEY=value` form, as the guest would see it.
    pub envp: Vec<Vec<u8>>,
    /// The working directory, absolute in the guest's namespace.
    pub cwd: String,
    /// Everything the command's standard input produced, already read to end of file.
    ///
    /// Empty unless [`HostCommand::reads_stdin`] returned true, because draining a standard
    /// input nobody is going to write to would hang.
    pub stdin: Vec<u8>,
}

impl Invocation {
    /// The arguments as text, lossily, which is what an option parser wants.
    pub fn args(&self) -> Vec<String> {
        self.argv
            .iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect()
    }

    /// The name the command was invoked as, from `argv[0]`.
    pub fn name(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(self.argv.first().map(Vec::as_slice).unwrap_or(b""))
    }

    /// Look up one environment variable.
    pub fn env(&self, key: &str) -> Option<std::borrow::Cow<'_, str>> {
        let prefix = format!("{key}=");
        self.envp
            .iter()
            .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
            .map(String::from_utf8_lossy)
    }
}

/// How a [`HostCommand`] finished.
#[derive(Debug, Default, Clone)]
pub struct Exit {
    /// The exit status, 0 to 255, as `$?` will report it.
    pub status: i32,
    /// Bytes for standard output.
    pub stdout: Vec<u8>,
    /// Bytes for standard error.
    pub stderr: Vec<u8>,
}

impl Exit {
    /// Success, with this on standard output.
    pub fn from_stdout(bytes: impl Into<Vec<u8>>) -> Exit {
        Exit {
            status: 0,
            stdout: bytes.into(),
            stderr: Vec::new(),
        }
    }

    /// A failure, with this on standard error. Conventionally status 1.
    pub fn failed(status: i32, message: impl Into<Vec<u8>>) -> Exit {
        Exit {
            status,
            stdout: Vec::new(),
            stderr: message.into(),
        }
    }
}

/// A command implemented in Rust rather than compiled to wasm.
///
/// See the [module documentation](self) for when to reach for this and what it costs.
pub trait HostCommand: Send + Sync + 'static {
    /// Do the work.
    ///
    /// Return `Err(Errno::AGAIN)` to suspend and be asked again with the same
    /// [`Invocation`]; any other error becomes that `errno` on the failed `execve`, which is
    /// almost never what you want — report a problem *the command* had as a non-zero
    /// [`Exit`] with a message on standard error, the way a program does.
    fn run(&self, call: &Invocation) -> VfsResult<Exit>;

    /// Whether this invocation reads standard input.
    ///
    /// False by default, and the default is the important case: a command that does not read
    /// standard input must not have it drained, or `curl https://example.com` as the first
    /// stage of a pipeline would sit waiting for a standard input nothing is going to close.
    ///
    /// It takes the arguments because for real tools the answer depends on them: `curl` reads
    /// standard input only for `-d @-`, and most filters only when given `-` or no file at
    /// all. Decide from `argv` and the kernel drains exactly when it should.
    fn reads_stdin(&self, argv: &[Vec<u8>]) -> bool {
        let _ = argv;
        false
    }
}
