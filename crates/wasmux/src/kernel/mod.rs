//! The kernel: process lifecycle, the scheduler, and the state syscalls act on.
//!
//! One [`Kernel`] is one session. It owns every running process, the shared tables they reach
//! across (descriptions, pipes, the process table) and the accounting that keeps a runaway
//! command from taking the host down with it.
//!
//! # How a process is suspended
//!
//! There is one thread, and a guest calls into the kernel from deep inside its own call stack.
//! Anything that cannot be answered at once, a read on an empty pipe, a `wait4` with no dead
//! child, a `Vfs` that said [`Errno::AGAIN`], is handled the same way: the kernel records what
//! the process was doing, asks Asyncify to unwind its stack into a scratch area, and returns
//! out of the guest entirely. The scheduler then runs somebody else. When the syscall can
//! finish, the stack is rewound, the import is re-entered, and it returns the value as if it
//! had blocked. `setjmp`, `longjmp`, `exit` and `execve` ride the same machinery.
//!
//! That is why the code below never blocks, never sleeps inside a syscall and never needs a
//! thread per process.

pub(crate) mod fd;
pub(crate) mod mem;
pub(crate) mod path;
pub(crate) mod syscall;
pub(crate) mod task;
/// The terminal a session may be given. Off unless the embedder asks for one.
#[cfg(feature = "tty")]
pub(crate) mod tty;

use crate::engine::{Exit, Instance, Program, Suspend, TrapKind};
use crate::slab::Slab;
use crate::vfs::Vfs;
use fd::{Desc, DescKey, DescKind, Fd, FdTable, Pipe, PipeKey};
use mem::Mem;
use path::{Mounts, Overlay, ProgramId, Resolver, Who};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use task::{ProcRecord, Task};

/// 64 KiB, the wasm page size, and the unit of memory growth.
pub(crate) const PAGE: u64 = 65536;

/// How much guest memory the Asyncify scratch area gets. A saved stack is a few hundred bytes
/// per frame, so this bounds recursion depth as much as the call-depth fuse does.
pub(crate) const ASYNCIFY_PAGES: u64 = 16;

/// What a session is allowed to consume.
///
/// Every limit is a ceiling the guest is told about honestly: memory becomes `ENOMEM`,
/// processes become `EAGAIN`, descriptors become `EMFILE`. Nothing here aborts the host.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Total guest memory across every live process, in bytes.
    pub memory: u64,
    /// Memory one process may reach, in bytes.
    pub memory_per_process: u64,
    /// Live processes at once.
    pub processes: usize,
    /// Open descriptors per process.
    pub open_files: usize,
    /// Bytes of standard output and standard error kept before truncating.
    pub output: usize,
    /// Wall-clock ceiling for the whole session, enforced at syscall boundaries.
    pub wall_clock: Option<Duration>,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            memory: 256 << 20,
            memory_per_process: 64 << 20,
            processes: 64,
            open_files: 256,
            output: 8 << 20,
            wall_clock: Some(Duration::from_secs(60)),
        }
    }
}

/// Everything a session shares between its processes.
///
/// Reached from a syscall through a raw pointer parked in [`Ctx::shared`]; see the safety note
/// there. Split out from the process list so that the two can be borrowed at once.
pub(crate) struct Shared {
    pub(crate) mounts: Arc<Mounts>,
    pub(crate) overlay: Arc<Overlay>,
    pub(crate) programs: Arc<Vec<ProgramEntry>>,
    pub(crate) limits: Limits,
    pub(crate) descs: Slab<Desc>,
    pub(crate) pipes: Slab<Pipe>,
    pub(crate) table: Vec<ProcRecord>,
    next_pid: i32,
    /// Children whose `execve` happened inside a parent's `vfork` window, waiting to be given
    /// an instance of their own by the scheduler.
    pub(crate) spawn: Vec<Spawn>,
    pub(crate) stdin: StdinBuffer,
    pub(crate) stdout: OutputBuffer,
    pub(crate) stderr: OutputBuffer,
    /// Guest memory currently allocated, in bytes.
    pub(crate) memory_used: u64,
    /// Set when a syscall could not finish because a [`Vfs`] returned [`Errno::AGAIN`], so the
    /// session can tell its caller to await something.
    pub(crate) vfs_pending: bool,
    /// A [`HostCommand`](crate::HostCommand) returned `AGAIN`. Same meaning as
    /// `vfs_pending`, reported separately so the caller knows which of its own things to
    /// await.
    pub(crate) host_pending: bool,
    pub(crate) started: Instant,
    /// Syscalls served, for accounting and tests.
    pub(crate) syscalls: u64,
    /// The terminal this session was given, if it was given one. `None` is the default and
    /// the only thing an agent should ever see: with no terminal, `isatty` says no and every
    /// terminal `ioctl` is `ENOTTY`, exactly as before this existed.
    #[cfg(feature = "tty")]
    pub(crate) terminal: Option<tty::Terminal>,
}

/// One program the sandbox can run, and where it appears in the namespace.
pub(crate) struct ProgramEntry {
    /// The image or command name, for diagnostics.
    #[allow(dead_code)]
    pub(crate) name: String,
    pub(crate) kind: ProgramKind,
}

/// The two ways a program can be implemented.
///
/// Both live in one table and share one id space, which is the reason host commands need no
/// support anywhere else: path resolution, `execve`, `/proc/self/exe`, `which` and the
/// program listing all see a program and do not care which kind it is.
pub(crate) enum ProgramKind {
    /// A wasm module: memory, Asyncify, syscalls.
    Guest(Arc<dyn Program>),
    /// Rust supplied by the embedder. See [`crate::HostCommand`].
    Host(Arc<dyn crate::HostCommand>),
}

/// Which kind of program `start` resolved, held by value so the borrow of the program table
/// ends before the process list is touched.
enum Either {
    Guest(Arc<dyn Program>),
    Host(Arc<dyn crate::HostCommand>),
}

/// A child waiting for the scheduler to give it an instance.
pub(crate) struct Spawn {
    pub(crate) task: Task,
    pub(crate) program: ProgramId,
    pub(crate) exe: String,
    pub(crate) argv: Vec<Vec<u8>>,
    pub(crate) envp: Vec<Vec<u8>>,
}

/// Bytes fed to the session's standard input.
#[derive(Default)]
pub(crate) struct StdinBuffer {
    pub(crate) data: std::collections::VecDeque<u8>,
    /// No more will arrive: reads return end of file.
    pub(crate) closed: bool,
    /// A process is blocked wanting more.
    pub(crate) wanted: bool,
}

/// Captured standard output or standard error, bounded.
#[derive(Default)]
pub(crate) struct OutputBuffer {
    pub(crate) data: Vec<u8>,
    /// Bytes dropped because the limit was reached.
    pub(crate) dropped: u64,
    /// Where the caller has already read up to, for streaming.
    pub(crate) taken: usize,
}

impl OutputBuffer {
    fn push(&mut self, bytes: &[u8], limit: usize) {
        let room = limit.saturating_sub(self.data.len());
        let take = room.min(bytes.len());
        if let Some(head) = bytes.get(..take) {
            self.data.extend_from_slice(head);
        }
        self.dropped = self
            .dropped
            .saturating_add((bytes.len().saturating_sub(take)) as u64);
    }

    /// Bytes not yet handed to the caller.
    pub(crate) fn drain_new(&mut self) -> Vec<u8> {
        let out = self.data.get(self.taken..).unwrap_or(&[]).to_vec();
        self.taken = self.data.len();
        out
    }
}

/// Why the guest's stack was unwound.
pub(crate) enum Suspension {
    /// `setjmp`: snapshot the stack for this `jmp_buf`, then rewind and return zero.
    Setjmp { env: u32, sp: u32 },
    /// `longjmp`: restore that snapshot, then rewind and return `val`.
    Longjmp { env: u32, val: i32 },
    /// A syscall that could not finish. Retried before the stack is rewound.
    Blocked { number: i32, args: [i32; 6] },
    /// The process is over.
    Exit(i32),
    /// `execve` succeeded: replace this process's instance.
    Exec,
}

/// Per-process state reachable from an import call.
///
/// The address is stable for the life of the process: the backend is handed a pointer to it at
/// instantiation and gives it back on every import.
pub(crate) struct Ctx {
    /// Valid only while this process is executing. The scheduler sets it before entering the
    /// guest and clears it afterwards, and nothing else holds a reference to [`Shared`] at
    /// that time, which is what makes dereferencing it sound.
    pub(crate) shared: *mut Shared,
    pub(crate) task: Task,
    /// Which program is running. Kept for diagnostics and for a future fast path that
    /// reuses an instance when `execve` names the image already loaded.
    #[allow(dead_code)]
    pub(crate) program: ProgramId,
    pub(crate) argv: Vec<Vec<u8>>,
    pub(crate) envp: Vec<Vec<u8>>,
    /// Set by `execve`, consumed by the scheduler.
    pub(crate) exec_request: Option<Spawn>,
    /// Identities of the vfork children currently running inside this instance.
    pub(crate) vfork_stack: Vec<Task>,
    pub(crate) exec: ExecState,
}

/// The Asyncify bookkeeping for one process.
#[derive(Default)]
pub(crate) struct ExecState {
    /// Base of the scratch area in guest memory.
    pub(crate) asyncify_data: u32,
    /// A rewind is in progress and the next import call is the one being resumed.
    pub(crate) rewinding: bool,
    /// What that import should return.
    pub(crate) resume_value: i32,
    /// The `__stack_pointer` to restore when the rewind lands.
    pub(crate) resume_sp: u32,
    /// Why the stack is being unwound, read by the scheduler once `_start` returns.
    pub(crate) suspension: Option<Suspension>,
    /// Saved stacks, keyed by the guest's `jmp_buf` address, with the stack pointer that went
    /// with each. Asyncify saves locals but not globals, so the pointer is saved separately.
    pub(crate) jmp_bufs: HashMap<u32, (Vec<u8>, u32)>,
    /// Where musl keeps its "a signal is pending" flag, from the `init` import.
    pub(crate) signal_flag: u32,
    /// Deadline of a sleep or a timed wait, kept across retries.
    pub(crate) deadline: Option<Instant>,
    /// The signal mask to put back when a `sigsuspend` finally returns. It has to stay in
    /// effect for as long as the call is blocked, because that mask is what decides which
    /// signal wakes it.
    pub(crate) mask_to_restore: Option<u64>,
}

impl Ctx {
    /// The shared state.
    ///
    /// The returned lifetime is deliberately not tied to `self`, so that a handler can hold
    /// the shared tables and its own task at the same time; they are different objects and
    /// the borrow checker cannot see that through the pointer.
    ///
    /// Two rules make it sound, and both are structural rather than hopeful: the pointer is
    /// only non-null while this process is executing, which is the only time a handler runs,
    /// and the scheduler holds no reference to [`Shared`] across a call into the guest. Do not
    /// keep the result across something that takes one of its own.
    #[allow(clippy::mut_from_ref)]
    pub(crate) fn shared<'a>(&self) -> &'a mut Shared {
        // SAFETY: see above. A null pointer here would mean a backend called an import while
        // the process was not scheduled, which no backend can do.
        unsafe { &mut *self.shared }
    }

    pub(crate) fn who(&self) -> Who<'_> {
        Who {
            cwd: &self.task.cwd,
            exe: &self.task.exe,
            pid: self.task.pid,
        }
    }
}

/// How a process continues the next time the scheduler reaches it.
enum Resume {
    /// Call `_start`.
    Fresh,
    /// Rewind the saved stack and re-enter the import that unwound it.
    Rewind,
    /// A syscall is outstanding: retry it, and only rewind once it can answer.
    Blocked { number: i32, args: [i32; 6] },
}

/// One running process: what it is, its context, and how to continue it.
struct Proc {
    ctx: Box<Ctx>,
    body: Body,
    resume: Resume,
    /// Guest memory charged to the accounting, so it can be returned on exit.
    charged: u64,
    /// Waiting on something outside the sandbox: a [`Vfs`] or a
    /// [`HostCommand`](crate::HostCommand) that answered [`Errno::AGAIN`].
    ///
    /// Such a process is not stepped again until the caller has been given control, because
    /// `AGAIN` means "come back later" and later has to mean *after* the caller could make
    /// progress. Without this the scheduler retries within the same slice, and an
    /// implementation that starts an HTTP request on each call would issue it in a spin.
    parked: bool,
}

/// What a process actually is.
enum Body {
    /// A wasm instance, driven through [`Instance`].
    Guest(Box<dyn Instance>),
    /// A host command, driven through [`HostProc`].
    Host(Box<HostProc>),
}

/// A host command's process state.
///
/// It is a small state machine rather than one call because both ends can block: standard
/// input may not have arrived yet, a pipe downstream may be full, and the command itself may
/// say `AGAIN` while it waits on something of the embedder's.
struct HostProc {
    command: Arc<dyn crate::HostCommand>,
    /// Whether the arguments say this invocation reads standard input.
    reads_stdin: bool,
    phase: Phase,
}

enum Phase {
    /// Draining standard input into the buffer the command will be handed.
    Reading(Vec<u8>),
    /// The command has produced its output; flushing it to the descriptors.
    ///
    /// A pipe can take it a piece at a time, so the cursors persist across steps.
    Flushing {
        status: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        out_done: usize,
        err_done: usize,
    },
}

/// What one pass over the process list achieved.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Slice {
    /// Something ran and there is more to do.
    #[allow(dead_code)]
    Progressed,
    /// The first process exited; the session is over.
    Finished(i32),
    /// Nothing can run until the world changes.
    Idle(Idle),
    /// The session's budget ran out. Call again when convenient.
    Budget,
}

/// Why nothing could run.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Idle {
    /// A [`HostCommand`](crate::HostCommand) returned [`Errno::AGAIN`].
    Host,
    /// A [`Vfs`] returned [`Errno::AGAIN`].
    Vfs,
    /// A process wants standard input.
    Stdin,
    /// Everyone is sleeping until this instant.
    Until(Instant),
    /// Nothing is runnable and nothing will change: a deadlock the kernel refuses to sit in.
    Deadlock,
}

/// One session's worth of running processes.
pub(crate) struct Kernel {
    pub(crate) shared: Shared,
    procs: Vec<Proc>,
    /// Wait status of the first process, once it has exited.
    exit_status: Option<i32>,
}

impl Kernel {
    pub(crate) fn new(
        mounts: Arc<Mounts>,
        overlay: Arc<Overlay>,
        programs: Arc<Vec<ProgramEntry>>,
        limits: Limits,
    ) -> Kernel {
        Kernel {
            shared: Shared {
                mounts,
                overlay,
                programs,
                limits,
                descs: Slab::new(),
                pipes: Slab::new(),
                table: Vec::new(),
                next_pid: 1,
                spawn: Vec::new(),
                stdin: StdinBuffer::default(),
                stdout: OutputBuffer::default(),
                stderr: OutputBuffer::default(),
                memory_used: 0,
                vfs_pending: false,
                host_pending: false,
                started: Instant::now(),
                syscalls: 0,
                #[cfg(feature = "tty")]
                terminal: None,
            },
            procs: Vec::new(),
            exit_status: None,
        }
    }

    /// Give this session a terminal of the given size. Before the first process starts.
    #[cfg(feature = "tty")]
    pub(crate) fn set_terminal(&mut self, cols: u16, rows: u16) {
        self.shared.terminal = Some(tty::Terminal::new(cols, rows));
    }

    /// Whether the guest has put the terminal into raw mode, so the host can match it.
    #[cfg(feature = "tty")]
    pub(crate) fn terminal_raw(&self) -> bool {
        self.shared
            .terminal
            .as_ref()
            .is_some_and(|t| t.termios.raw())
    }

    /// Whether the guest still wants Ctrl-C to raise a signal rather than arrive as a byte.
    #[cfg(feature = "tty")]
    pub(crate) fn terminal_signals(&self) -> bool {
        self.shared
            .terminal
            .as_ref()
            .is_some_and(|t| t.termios.signals())
    }

    /// Start the first process. Its descriptors are standard input, output and error.
    pub(crate) fn spawn_first(
        &mut self,
        program: ProgramId,
        exe: String,
        argv: Vec<Vec<u8>>,
        envp: Vec<Vec<u8>>,
        cwd: String,
    ) -> Result<(), crate::engine::LoadError> {
        let pid = self.shared.allocate_pid();
        let mut task = Task::new(pid, FdTable::new());
        task.cwd = cwd;
        let stdio = [
            (DescKind::Stdin, "/dev/stdin", crate::abi::O_RDONLY),
            (DescKind::Stdout(1), "/dev/stdout", crate::abi::O_WRONLY),
            (DescKind::Stdout(2), "/dev/stderr", crate::abi::O_WRONLY),
        ];
        for (kind, path, flags) in stdio {
            let key = self
                .shared
                .descs
                .insert(
                    Desc::new(kind, path, flags),
                    self.shared.limits.open_files.saturating_mul(4),
                )
                .ok_or(crate::engine::LoadError::OutOfMemory)?;
            let _ = task.fds.alloc(
                Fd {
                    desc: key,
                    cloexec: false,
                },
                0,
                self.shared.limits.open_files,
            );
        }
        let comm = argv
            .first()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .unwrap_or_default();
        self.shared
            .table
            .push(ProcRecord::new(pid, 0, pid, pid, comm));
        self.start(Spawn {
            task,
            program,
            exe,
            argv,
            envp,
        })
    }

    /// Turn a pending spawn into a running process.
    fn start(&mut self, spawn: Spawn) -> Result<(), crate::engine::LoadError> {
        let Spawn {
            mut task,
            program,
            exe,
            argv,
            envp,
        } = spawn;
        task.exe = exe;
        task.comm = argv
            .first()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .unwrap_or_default();
        task.reset_handlers_for_exec();
        let entry = self
            .shared
            .programs
            .get(program)
            .ok_or(crate::engine::LoadError::NotWasm)?;
        let kind = match &entry.kind {
            ProgramKind::Guest(module) => Either::Guest(module.clone()),
            ProgramKind::Host(command) => Either::Host(command.clone()),
        };
        let mut ctx = Box::new(Ctx {
            shared: core::ptr::null_mut(),
            task,
            program,
            argv,
            envp,
            exec_request: None,
            vfork_stack: Vec::new(),
            exec: ExecState::default(),
        });
        // A host command has no memory, no Asyncify area and nothing to charge: it is a
        // function, and the process record around it is all the state there is.
        let module = match kind {
            Either::Host(command) => {
                let reads_stdin = command.reads_stdin(&ctx.argv);
                self.procs.push(Proc {
                    ctx,
                    body: Body::Host(Box::new(HostProc {
                        command,
                        reads_stdin,
                        phase: Phase::Reading(Vec::new()),
                    })),
                    resume: Resume::Fresh,
                    charged: 0,
                    parked: false,
                });
                return Ok(());
            }
            Either::Guest(module) => module,
        };
        let ctx_ptr = (&mut *ctx) as *mut Ctx as *mut core::ffi::c_void;
        let mut instance = module.instantiate(ctx_ptr)?;
        // Reserve the Asyncify scratch area at the top of the guest's memory.
        let base = instance.mem_size();
        let charge = base.saturating_add(ASYNCIFY_PAGES.saturating_mul(PAGE));
        if !self.shared.charge_memory(charge) {
            return Err(crate::engine::LoadError::OutOfMemory);
        }
        match instance.mem_grow(ASYNCIFY_PAGES) {
            Some(old) => ctx.exec.asyncify_data = (old.saturating_mul(PAGE)) as u32,
            None => {
                self.shared.release_memory(charge);
                return Err(crate::engine::LoadError::OutOfMemory);
            }
        }
        self.procs.push(Proc {
            ctx,
            body: Body::Guest(instance),
            resume: Resume::Fresh,
            charged: charge,
            parked: false,
        });
        Ok(())
    }

    /// Run until something has to be reported. `budget` bounds the syscalls served, so that a
    /// caller with an event loop gets it back promptly.
    pub(crate) fn run(&mut self, budget: Budget) -> Slice {
        let start_syscalls = self.shared.syscalls;
        // The caller has had control since the last slice, so whatever a parked process was
        // waiting on has had its chance to progress. Exactly once per step: clearing this
        // inside the loop below would be the same as not having it.
        for proc in &mut self.procs {
            proc.parked = false;
        }
        loop {
            // Give any process created by a vfork+exec an instance of its own.
            let pending: Vec<Spawn> = core::mem::take(&mut self.shared.spawn);
            for spawn in pending {
                let pid = spawn.task.pid;
                if let Err(e) = self.start(spawn) {
                    // The child cannot run: report it the way a failed exec does.
                    self.shared.set_exit(pid, task::status_exited(126));
                    let _ = e;
                }
            }

            if let Some(deadline) = self.wall_clock_deadline() {
                if Instant::now() >= deadline {
                    self.kill_all(task::status_signaled(crate::abi::SIGKILL));
                    return Slice::Finished(
                        self.exit_status
                            .unwrap_or(task::status_signaled(crate::abi::SIGKILL)),
                    );
                }
            }

            let mut progressed = false;
            let mut index = 0;
            while index < self.procs.len() {
                // Whether *this* process is the one that asked to be retried later. Read
                // either side of the step because the flags are also what tells the caller
                // why the slice ended, and they are consumed there rather than here.
                let waited = (self.shared.vfs_pending, self.shared.host_pending);
                let step = self.step_process(index);
                let external = (self.shared.vfs_pending && !waited.0)
                    || (self.shared.host_pending && !waited.1);
                if external {
                    if let Some(proc) = self.procs.get_mut(index) {
                        proc.parked = true;
                    }
                }
                match step {
                    ProcStep::Ran => {
                        progressed = true;
                        index = index.saturating_add(1);
                    }
                    ProcStep::Stuck => index = index.saturating_add(1),
                    ProcStep::Ended(status) => {
                        progressed = true;
                        self.finish_process(index, status);
                        if self.exit_status.is_some() {
                            return Slice::Finished(self.exit_status.unwrap_or(0));
                        }
                    }
                    ProcStep::Replaced => {
                        progressed = true;
                        index = index.saturating_add(1);
                    }
                }
            }

            if self.procs.is_empty() {
                let status = self.exit_status.unwrap_or(0);
                return Slice::Finished(status);
            }
            if !progressed {
                return Slice::Idle(self.why_idle());
            }
            if budget.spent(
                self.shared.syscalls.saturating_sub(start_syscalls),
                self.shared.started,
            ) {
                return Slice::Budget;
            }
        }
    }

    fn wall_clock_deadline(&self) -> Option<Instant> {
        self.shared
            .limits
            .wall_clock
            .and_then(|budget| self.shared.started.checked_add(budget))
    }

    /// Advance one process as far as it will go without blocking.
    fn step_process(&mut self, index: usize) -> ProcStep {
        // The raw pointer is taken before borrowing the process list, so no reference to
        // `Shared` is alive while the guest runs. See `Ctx::shared`.
        let shared: *mut Shared = &mut self.shared;
        let Some(proc) = self.procs.get_mut(index) else {
            return ProcStep::Stuck;
        };
        if proc.parked {
            return ProcStep::Stuck;
        }
        proc.ctx.shared = shared;
        if matches!(proc.body, Body::Host(_)) {
            let step = self.step_host(index);
            if let Some(proc) = self.procs.get_mut(index) {
                proc.ctx.shared = core::ptr::null_mut();
            }
            return step;
        }
        let Body::Guest(instance) = &mut proc.body else {
            return ProcStep::Stuck;
        };

        match core::mem::replace(&mut proc.resume, Resume::Fresh) {
            Resume::Fresh => {}
            Resume::Rewind => {
                let data = proc.ctx.exec.asyncify_data;
                proc.ctx.exec.rewinding = true;
                instance.asy_start_rewind(data);
            }
            Resume::Blocked { number, args } => {
                // Retry outside the guest: the same handler, with the instance standing in
                // for the calling context. It cannot unwind again, and does not need to.
                let mut suspend = InstanceSuspend(&mut **instance);
                match syscall::dispatch(&mut proc.ctx, &mut suspend, number, args) {
                    syscall::Outcome::Blocked => {
                        proc.resume = Resume::Blocked { number, args };
                        proc.ctx.shared = core::ptr::null_mut();
                        return ProcStep::Stuck;
                    }
                    syscall::Outcome::Exit(status) => {
                        proc.ctx.shared = core::ptr::null_mut();
                        return ProcStep::Ended(status);
                    }
                    syscall::Outcome::Exec => {
                        proc.ctx.shared = core::ptr::null_mut();
                        return self.replace_instance(index);
                    }
                    syscall::Outcome::Value(v) => {
                        proc.ctx.exec.resume_value = v;
                        proc.ctx.exec.rewinding = true;
                        let data = proc.ctx.exec.asyncify_data;
                        instance.asy_start_rewind(data);
                    }
                }
            }
        }

        let exit = instance.run_start();
        proc.ctx.shared = core::ptr::null_mut();

        match exit {
            Exit::Trap(kind) => {
                let sh = unsafe { &mut *shared };
                sh.stderr.push(
                    format!("wasmux: {}: {}\n", proc.ctx.task.comm, kind.as_str()).as_bytes(),
                    sh.limits.output,
                );
                ProcStep::Ended(task::status_signaled(match kind {
                    TrapKind::Exhaustion => crate::abi::SIGSEGV,
                    _ => crate::abi::SIGSEGV,
                }))
            }
            Exit::Returned => self.after_return(index),
        }
    }

    /// Advance a host command.
    ///
    /// Three things can stop it, and all three are ordinary suspension rather than anything
    /// new: standard input has not arrived, the command said `AGAIN`, or a pipe downstream is
    /// full. The reads and writes go through [`syscall::read_bytes`] and
    /// [`syscall::write_bytes`], which is why redirection, pipelines and
    /// [`Limits::output`](crate::Limits::output) apply to it exactly as they do to a guest.
    fn step_host(&mut self, index: usize) -> ProcStep {
        let Some(proc) = self.procs.get_mut(index) else {
            return ProcStep::Stuck;
        };
        let Body::Host(host) = &mut proc.body else {
            return ProcStep::Stuck;
        };

        // Rebind through raw parts so the command can be called while `proc.ctx` is borrowed
        // for its descriptors: the command is an `Arc` and the phase is separate state.
        if let Phase::Reading(buffer) = &mut host.phase {
            if host.reads_stdin {
                let mut chunk = [0u8; 8192];
                loop {
                    match syscall::read_bytes(&mut proc.ctx, 0, &mut chunk) {
                        Ok(0) => break,
                        Ok(n) => buffer.extend_from_slice(chunk.get(..n).unwrap_or(&[])),
                        Err(syscall::Fault::Block) => return ProcStep::Stuck,
                        // A standard input that cannot be read is an empty one, the same
                        // thing a program with a closed descriptor sees.
                        Err(_) => break,
                    }
                }
            }
            let call = crate::Invocation {
                argv: proc.ctx.argv.clone(),
                envp: proc.ctx.envp.clone(),
                cwd: proc.ctx.task.cwd.clone(),
                stdin: core::mem::take(buffer),
            };
            let command = host.command.clone();
            match command.run(&call) {
                Ok(exit) => {
                    host.phase = Phase::Flushing {
                        status: task::status_exited(exit.status & 0xff),
                        stdout: exit.stdout,
                        stderr: exit.stderr,
                        out_done: 0,
                        err_done: 0,
                    };
                }
                Err(crate::Errno::AGAIN) => {
                    // The command started something of the embedder's. Put the input back so
                    // the retry is handed the identical invocation, as the contract promises.
                    host.phase = Phase::Reading(call.stdin);
                    self.shared.host_pending = true;
                    return ProcStep::Stuck;
                }
                Err(e) => {
                    // Not a failure of the command but of running it at all. Report it the
                    // way a shell reports one, so the model sees a reason.
                    let message = format!("wasmux: {}: {}\n", proc.ctx.task.comm, e.name());
                    host.phase = Phase::Flushing {
                        status: task::status_exited(126),
                        stdout: Vec::new(),
                        stderr: message.into_bytes(),
                        out_done: 0,
                        err_done: 0,
                    };
                }
            }
        }

        let Some(proc) = self.procs.get_mut(index) else {
            return ProcStep::Stuck;
        };
        let Body::Host(host) = &mut proc.body else {
            return ProcStep::Stuck;
        };
        let Phase::Flushing {
            status,
            stdout,
            stderr,
            out_done,
            err_done,
        } = &mut host.phase
        else {
            return ProcStep::Stuck;
        };

        // Standard output first, then standard error, each resumable where it stopped.
        for (fd, bytes, done) in [(1, &*stdout, out_done), (2, &*stderr, err_done)] {
            while *done < bytes.len() {
                let rest = bytes.get(*done..).unwrap_or(&[]);
                match syscall::write_bytes(&mut proc.ctx, fd, rest) {
                    Ok(0) => break,
                    Ok(n) => *done = done.saturating_add(n),
                    Err(syscall::Fault::Block) => return ProcStep::Stuck,
                    // SIGPIPE, a closed descriptor, a full disk: the same as any program
                    // writing into one. Stop writing this stream and finish.
                    Err(_) => break,
                }
            }
        }
        ProcStep::Ended(*status)
    }

    /// `_start` came back: either the program is done, or an import unwound its stack.
    fn after_return(&mut self, index: usize) -> ProcStep {
        let Some(proc) = self.procs.get_mut(index) else {
            return ProcStep::Stuck;
        };
        let Some(suspension) = proc.ctx.exec.suspension.take() else {
            // main returned without calling exit: status zero.
            return ProcStep::Ended(task::status_exited(0));
        };
        // Only a guest reaches here: a host command never unwinds a stack it does not have.
        let Body::Guest(instance) = &mut proc.body else {
            return ProcStep::Stuck;
        };
        instance.asy_stop_unwind();
        let data = proc.ctx.exec.asyncify_data;
        match suspension {
            Suspension::Setjmp { env, sp } => {
                let snapshot = read_asyncify_stack(instance.as_ref(), data);
                proc.ctx.exec.jmp_bufs.insert(env, (snapshot, sp));
                proc.ctx.exec.resume_value = 0;
                proc.ctx.exec.resume_sp = sp;
                proc.resume = Resume::Rewind;
                ProcStep::Ran
            }
            Suspension::Longjmp { env, val } => {
                let Some((snapshot, sp)) = proc.ctx.exec.jmp_bufs.get(&env).cloned() else {
                    // longjmp to a jmp_buf that was never set: the guest's bug, its death.
                    return ProcStep::Ended(task::status_signaled(crate::abi::SIGSEGV));
                };
                if !write_asyncify_stack(instance.as_mut(), data, &snapshot) {
                    return ProcStep::Ended(task::status_signaled(crate::abi::SIGSEGV));
                }
                proc.ctx.exec.resume_value = val;
                proc.ctx.exec.resume_sp = sp;
                proc.resume = Resume::Rewind;
                ProcStep::Ran
            }
            Suspension::Blocked { number, args } => {
                proc.resume = Resume::Blocked { number, args };
                ProcStep::Ran
            }
            Suspension::Exit(status) => ProcStep::Ended(status),
            Suspension::Exec => self.replace_instance(index),
        }
    }

    /// `execve` in a process that was not a vfork child: swap the instance in place, keeping
    /// the pid, the descriptors and the parent.
    fn replace_instance(&mut self, index: usize) -> ProcStep {
        let Some(proc) = self.procs.get_mut(index) else {
            return ProcStep::Stuck;
        };
        let Some(spawn) = proc.ctx.exec_request.take() else {
            return ProcStep::Ended(task::status_exited(126));
        };
        let charged = proc.charged;
        let old = self.procs.remove(index);
        drop(old);
        self.shared.release_memory(charged);
        let pid = spawn.task.pid;
        match self.start(spawn) {
            Ok(()) => {
                // `start` pushed at the end; move it back so scheduling order is stable.
                if let Some(new) = self.procs.pop() {
                    self.procs.insert(index.min(self.procs.len()), new);
                }
                ProcStep::Replaced
            }
            Err(_) => {
                self.shared.set_exit(pid, task::status_exited(126));
                ProcStep::Stuck
            }
        }
    }

    /// Tear a dead process down: release its descriptors, record the status, wake its parent.
    fn finish_process(&mut self, index: usize, status: i32) {
        if index >= self.procs.len() {
            return;
        }
        let proc = self.procs.remove(index);
        let Proc {
            mut ctx,
            body,
            charged,
            ..
        } = proc;
        drop(body);
        self.shared.release_memory(charged);
        // A vfork child still inside this instance dies with it.
        while let Some(parent) = ctx.vfork_stack.pop() {
            let child = core::mem::replace(&mut ctx.task, parent);
            let pid = child.pid;
            self.shared.release_fds(child.fds);
            self.shared
                .set_exit(pid, task::status_signaled(crate::abi::SIGKILL));
        }
        let pid = ctx.task.pid;
        self.shared.release_fds(core::mem::take(&mut ctx.task.fds));
        self.shared.set_exit(pid, status);
        if pid == 1 || self.procs.is_empty() {
            self.exit_status.get_or_insert(status);
        }
    }

    fn kill_all(&mut self, status: i32) {
        while !self.procs.is_empty() {
            self.finish_process(0, status);
        }
        self.exit_status.get_or_insert(status);
    }

    /// Nothing ran: work out what the caller has to do about it.
    fn why_idle(&mut self) -> Idle {
        if core::mem::take(&mut self.shared.vfs_pending) {
            return Idle::Vfs;
        }
        if core::mem::take(&mut self.shared.host_pending) {
            return Idle::Host;
        }
        let mut earliest: Option<Instant> = None;
        for record in &self.shared.table {
            if let Some(when) = record.wake_at {
                earliest = Some(match earliest {
                    Some(current) if current <= when => current,
                    _ => when,
                });
            }
        }
        if let Some(when) = earliest {
            return Idle::Until(when);
        }
        if self.shared.stdin.wanted && !self.shared.stdin.closed {
            return Idle::Stdin;
        }
        Idle::Deadlock
    }

    pub(crate) fn processes(&self) -> usize {
        self.procs.len()
    }
}

/// Lets the scheduler serve a retried syscall through the same handlers the guest uses.
struct InstanceSuspend<'a>(&'a mut dyn Instance);

impl Suspend for InstanceSuspend<'_> {
    fn mem(&mut self) -> (*mut u8, usize) {
        self.0.mem()
    }
    fn mem_grow(&mut self, pages: u64) -> Option<u64> {
        self.0.mem_grow(pages)
    }
    fn stack_pointer(&mut self) -> u32 {
        self.0.stack_pointer()
    }
    fn set_stack_pointer(&mut self, value: u32) {
        self.0.set_stack_pointer(value);
    }
    fn start_unwind(&mut self, data: u32) {
        self.0.asy_start_unwind(data);
    }
    fn stop_rewind(&mut self) {
        self.0.asy_stop_rewind();
    }
}

enum ProcStep {
    Ran,
    Stuck,
    Ended(i32),
    Replaced,
}

/// How much work one call to the scheduler may do.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    /// Syscalls to serve before handing control back.
    pub syscalls: Option<u64>,
    /// Wall-clock slice.
    pub duration: Option<Duration>,
}

impl Budget {
    /// Run until the session finishes or blocks.
    pub const fn unlimited() -> Budget {
        Budget {
            syscalls: None,
            duration: None,
        }
    }
    /// Hand control back after roughly `n` syscalls.
    pub const fn syscalls(n: u64) -> Budget {
        Budget {
            syscalls: Some(n),
            duration: None,
        }
    }
    /// Hand control back after roughly `d`.
    pub const fn duration(d: Duration) -> Budget {
        Budget {
            syscalls: None,
            duration: Some(d),
        }
    }

    fn spent(&self, served: u64, started: Instant) -> bool {
        if let Some(limit) = self.syscalls {
            if served >= limit {
                return true;
            }
        }
        if let Some(limit) = self.duration {
            if started.elapsed() >= limit {
                return true;
            }
        }
        false
    }
}

impl Shared {
    fn allocate_pid(&mut self) -> i32 {
        let pid = self.next_pid;
        self.next_pid = self.next_pid.saturating_add(1);
        pid
    }

    /// A new pid for a `vfork` child, with a record of its own.
    pub(crate) fn fork_record(&mut self, parent: &Task) -> Option<i32> {
        let live = self.table.iter().filter(|r| r.is_alive()).count();
        if live >= self.limits.processes {
            return None;
        }
        let pid = self.allocate_pid();
        let (ppid, pgid, sid) = match self.record(parent.pid) {
            Some(r) => (r.pid, r.pgid, r.sid),
            None => (parent.pid, parent.pid, parent.pid),
        };
        self.table
            .push(ProcRecord::new(pid, ppid, pgid, sid, parent.comm.clone()));
        Some(pid)
    }

    pub(crate) fn record(&self, pid: i32) -> Option<&ProcRecord> {
        self.table.iter().find(|r| r.pid == pid)
    }

    pub(crate) fn record_mut(&mut self, pid: i32) -> Option<&mut ProcRecord> {
        self.table.iter_mut().find(|r| r.pid == pid)
    }

    /// Record an exit, reparent any orphans, and tell the parent.
    pub(crate) fn set_exit(&mut self, pid: i32, status: i32) {
        let mut parent = 0;
        if let Some(record) = self.record_mut(pid) {
            if record.exit.is_none() {
                record.exit = Some(status);
            }
            record.wake_at = None;
            parent = record.ppid;
        }
        for record in self.table.iter_mut() {
            if record.ppid == pid {
                record.ppid = 1;
            }
        }
        let parent_alive = self.record(parent).map(|r| r.is_alive()).unwrap_or(false);
        if parent_alive {
            if let Some(record) = self.record_mut(parent) {
                record.raise(crate::abi::SIGCHLD);
            }
        } else {
            // Nobody will wait for it, so do not keep a zombie around.
            self.table.retain(|r| r.pid != pid);
        }
    }

    /// Charge guest memory against the session's limit.
    pub(crate) fn charge_memory(&mut self, bytes: u64) -> bool {
        let next = self.memory_used.saturating_add(bytes);
        if next > self.limits.memory {
            return false;
        }
        self.memory_used = next;
        true
    }

    pub(crate) fn release_memory(&mut self, bytes: u64) {
        self.memory_used = self.memory_used.saturating_sub(bytes);
    }

    /// Drop one reference to a description, closing it when the last goes.
    pub(crate) fn release_desc(&mut self, key: DescKey) {
        let done = match self.descs.get_mut(key) {
            Some(desc) => {
                desc.refs = desc.refs.saturating_sub(1);
                desc.refs == 0
            }
            None => false,
        };
        if !done {
            return;
        }
        if let Some(mut desc) = self.descs.remove(key) {
            match &mut desc.kind {
                DescKind::File { file, closed, .. } => {
                    if !*closed {
                        // A close that wants to block cannot be retried here; the write-back
                        // has to happen eagerly. Integrators are told so in the trait docs.
                        let _ = file.close();
                        *closed = true;
                    }
                }
                DescKind::PipeRead(key) => {
                    if let Some(pipe) = self.pipes.get_mut(*key) {
                        pipe.readers = pipe.readers.saturating_sub(1);
                    }
                    self.collect_pipe(*key);
                }
                DescKind::PipeWrite(key) => {
                    if let Some(pipe) = self.pipes.get_mut(*key) {
                        pipe.writers = pipe.writers.saturating_sub(1);
                    }
                    self.collect_pipe(*key);
                }
                _ => {}
            }
        }
    }

    fn collect_pipe(&mut self, key: PipeKey) {
        let gone = self
            .pipes
            .get(key)
            .map(|p| p.readers == 0 && p.writers == 0)
            .unwrap_or(false);
        if gone {
            self.pipes.remove(key);
        }
    }

    /// Release every descriptor of a dying process.
    pub(crate) fn release_fds(&mut self, mut table: FdTable) {
        for key in table.drain_all() {
            self.release_desc(key);
        }
    }

    /// Append to the session's standard output or error.
    pub(crate) fn write_output(&mut self, which: u8, bytes: &[u8]) -> usize {
        let limit = self.limits.output;
        if which == 2 {
            self.stderr.push(bytes, limit);
        } else {
            self.stdout.push(bytes, limit);
        }
        bytes.len()
    }

    pub(crate) fn resolver(&self) -> Resolver<'_> {
        Resolver {
            overlay: &self.overlay,
            mounts: &self.mounts,
        }
    }

    /// Whether a mount refuses writes.
    pub(crate) fn is_read_only(&self, mount: usize) -> bool {
        self.mounts.get(mount).map(|m| m.read_only).unwrap_or(true)
    }

    pub(crate) fn vfs(&self, mount: usize) -> Option<&Arc<dyn Vfs>> {
        self.mounts.get(mount).map(|m| &m.vfs)
    }

    /// Note that a `Vfs` asked to be called again.
    pub(crate) fn note_vfs_pending(&mut self) {
        self.vfs_pending = true;
    }
}

/// Copy the saved stack out of the guest's Asyncify area.
///
/// The header's first word is the current position, so only the used part is copied, which is
/// a few hundred bytes for a shell rather than the whole reserved area.
fn read_asyncify_stack(instance: &dyn Instance, data: u32) -> Vec<u8> {
    let (base, len) = instance.mem();
    // SAFETY: the guest is suspended and the region belongs to it.
    let memory = unsafe { core::slice::from_raw_parts(base, len) };
    let start = data as usize;
    let Some(header) = memory.get(start..start.saturating_add(8)) else {
        return Vec::new();
    };
    let Ok(word) = header.get(..4).unwrap_or(&[]).try_into() else {
        return Vec::new();
    };
    let end = u32::from_le_bytes(word) as usize;
    if end <= start {
        return Vec::new();
    }
    memory.get(start..end).unwrap_or(&[]).to_vec()
}

/// Put a saved stack back, ready for a rewind.
fn write_asyncify_stack(instance: &mut dyn Instance, data: u32, snapshot: &[u8]) -> bool {
    let (base, len) = instance.mem();
    // SAFETY: as above; the guest is suspended.
    let memory = unsafe { core::slice::from_raw_parts_mut(base, len) };
    let start = data as usize;
    let end = start.saturating_add(snapshot.len());
    match memory.get_mut(start..end) {
        Some(target) => {
            target.copy_from_slice(snapshot);
            true
        }
        None => false,
    }
}

/// The import shims, shared by every backend.
///
/// A backend turns its own calling convention into these six calls and nothing more.
pub(crate) mod imports {
    use super::{syscall, Ctx, Mem, Suspension};
    use crate::engine::Suspend;

    /// `wasmux.syscall(number, args) -> result`
    pub(crate) fn syscall_entry(
        ctx: &mut Ctx,
        s: &mut dyn Suspend,
        number: u32,
        args_ptr: u32,
    ) -> u32 {
        if ctx.exec.rewinding {
            return finish_rewind(ctx, s) as u32;
        }
        let mut args = [0i32; 6];
        {
            let (base, len) = s.mem();
            // SAFETY: the backend's memory, and the guest is inside this call.
            let memory = unsafe { Mem::new(base, len) };
            for (index, slot) in args.iter_mut().enumerate() {
                let at = args_ptr.wrapping_add((index as u32).wrapping_mul(4));
                match memory.u32(at) {
                    Ok(value) => *slot = value as i32,
                    Err(e) => return e.as_neg() as u32,
                }
            }
        }
        let number = number as i32;
        match syscall::dispatch(ctx, s, number, args) {
            syscall::Outcome::Value(v) => v as u32,
            syscall::Outcome::Blocked => {
                let sp = s.stack_pointer();
                ctx.exec.resume_sp = sp;
                unwind(ctx, s, Suspension::Blocked { number, args });
                0
            }
            syscall::Outcome::Exit(status) => {
                unwind(ctx, s, Suspension::Exit(status));
                0
            }
            syscall::Outcome::Exec => {
                unwind(ctx, s, Suspension::Exec);
                0
            }
        }
    }

    /// `wasmux.setjmp(jmp_buf) -> value`
    pub(crate) fn setjmp_entry(ctx: &mut Ctx, s: &mut dyn Suspend, env: u32) -> u32 {
        if ctx.exec.rewinding {
            return finish_rewind(ctx, s) as u32;
        }
        let sp = s.stack_pointer();
        unwind(ctx, s, Suspension::Setjmp { env, sp });
        0
    }

    /// `wasmux.longjmp(jmp_buf, value)`
    pub(crate) fn longjmp_entry(ctx: &mut Ctx, s: &mut dyn Suspend, env: u32, value: i32) {
        unwind(ctx, s, Suspension::Longjmp { env, val: value });
    }

    /// `wasmux.init(signal_flag)`: musl tells the kernel where its pending-signal flag lives.
    pub(crate) fn init_entry(ctx: &mut Ctx, signal_flag: u32) {
        ctx.exec.signal_flag = signal_flag;
    }

    /// `wasmux.args(buffer, capacity) -> size`: the startup block, or its size when capacity
    /// is zero.
    pub(crate) fn args_entry(
        ctx: &mut Ctx,
        s: &mut dyn Suspend,
        buffer: u32,
        capacity: u32,
    ) -> u32 {
        let block = syscall::start_block(ctx, buffer);
        if capacity == 0 {
            return block.len() as u32;
        }
        if (capacity as usize) < block.len() {
            return crate::errno::Errno::TOOBIG.as_neg() as u32;
        }
        let (base, len) = s.mem();
        // SAFETY: the backend's memory, and the guest is inside this call.
        let mut memory = unsafe { Mem::new(base, len) };
        match memory.put(buffer, &block) {
            Ok(()) => block.len() as u32,
            Err(e) => e.as_neg() as u32,
        }
    }

    /// `wasmux.sigfetch(out) -> 1 if a handler should run`
    pub(crate) fn sigfetch_entry(ctx: &mut Ctx, s: &mut dyn Suspend, out: u32) -> u32 {
        match syscall::next_signal(ctx, s, out) {
            syscall::SignalDelivery::None => 0,
            syscall::SignalDelivery::Handler => 1,
            syscall::SignalDelivery::Fatal(status) => {
                unwind(ctx, s, Suspension::Exit(status));
                0
            }
        }
    }

    /// Record why we are leaving and ask Asyncify to save the stack.
    fn unwind(ctx: &mut Ctx, s: &mut dyn Suspend, why: Suspension) {
        if std::env::var("WASMUX_TRACE").is_ok() {
            let what = match &why {
                Suspension::Setjmp { env, .. } => format!("setjmp {env:#x}"),
                Suspension::Longjmp { env, val } => format!("longjmp {env:#x} -> {val}"),
                Suspension::Blocked { number, .. } => {
                    format!("block in {}", crate::nr::name(*number))
                }
                Suspension::Exit(status) => format!("exit {status:#x}"),
                Suspension::Exec => "exec".to_string(),
            };
            eprintln!("[wasmux {}] unwind: {what}", ctx.task.pid);
        }
        let data = ctx.exec.asyncify_data;
        let size = super::ASYNCIFY_PAGES
            .saturating_mul(super::PAGE)
            .saturating_sub(8) as u32;
        let (base, len) = s.mem();
        // SAFETY: the backend's memory, and the guest is inside this call.
        let mut memory = unsafe { Mem::new(base, len) };
        // The Asyncify header is two words: where to write next, and where to stop.
        let _ = memory.put_u32(data, data.saturating_add(8));
        let _ = memory.put_u32(
            data.saturating_add(4),
            data.saturating_add(8).saturating_add(size),
        );
        ctx.exec.suspension = Some(why);
        s.start_unwind(data);
    }

    /// The rewind reached the import that unwound: stop, restore the stack pointer, and hand
    /// back the value the import should have returned.
    fn finish_rewind(ctx: &mut Ctx, s: &mut dyn Suspend) -> i32 {
        s.stop_rewind();
        ctx.exec.rewinding = false;
        let sp = ctx.exec.resume_sp;
        if sp != 0 {
            s.set_stack_pointer(sp);
        }
        ctx.exec.resume_value
    }
}
