//! The syscall table: what the guest asks for, and what the kernel does about it.
//!
//! Every handler is a pure function of the kernel's state and the guest's memory, which is
//! what makes them testable without an engine and what keeps the blocking story simple: a
//! handler that cannot finish returns [`Fault::Block`] and is called again later with the same
//! arguments, so it must be idempotent up to the point where it commits.
//!
//! Nothing here panics. Bounds are checked, arithmetic saturates or is checked, and a bad
//! pointer from the guest is [`Errno::FAULT`], not an abort.

use super::fd::{dirent_kind, Desc, DescKind, DirEnt, Fd, Pipe};
use super::mem::Mem;
use super::path::{Device, Node};
use super::task::{self, SigAction, NSIG};
use super::{Ctx, Shared, Spawn};
use crate::abi::*;
use crate::engine::Suspend;
use crate::errno::Errno;
use crate::nr;
use crate::vfs::{FileType, OpenOptions, SeekFrom, Stat};
use std::time::{Duration, Instant, SystemTime};

/// What a syscall did, as the import shim and the scheduler see it.
pub(crate) enum Outcome {
    /// Hand this back to the guest. Negative values are errors.
    Value(i32),
    /// Could not finish; retry after something changes.
    Blocked,
    /// The process is over with this wait status.
    Exit(i32),
    /// `execve` succeeded; the scheduler replaces the instance.
    Exec,
}

/// The error channel inside the handlers.
pub(crate) enum Fault {
    Errno(Errno),
    Block,
    Exit(i32),
    Exec,
}

impl From<Errno> for Fault {
    fn from(e: Errno) -> Fault {
        Fault::Errno(e)
    }
}

type Sys = Result<i32, Fault>;

/// Guest memory for the duration of one handler.
///
/// # Safety
///
/// The result must not be held across [`Suspend::mem_grow`], which may move the buffer. Every
/// site that grows takes a fresh one afterwards.
fn memory<'a>(s: &mut dyn Suspend) -> Mem<'a> {
    let (base, len) = s.mem();
    // SAFETY: the backend guarantees the region, and the guest is suspended inside this call.
    unsafe { Mem::new(base, len) }
}

/// Turn a `Vfs` error into a fault, treating [`Errno::AGAIN`] as "come back later".
fn vfs_fault(sh: &mut Shared, e: Errno) -> Fault {
    if e == Errno::AGAIN {
        sh.note_vfs_pending();
        Fault::Block
    } else {
        Fault::Errno(e)
    }
}

/// Whether to print a line per syscall. Set `WASMUX_TRACE=1` to turn it on; it is the first
/// thing to reach for when a guest does something surprising.
fn tracing() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("WASMUX_TRACE").is_ok_and(|v| v != "0"))
}

/// Serve one syscall.
pub(crate) fn dispatch(ctx: &mut Ctx, s: &mut dyn Suspend, number: i32, args: [i32; 6]) -> Outcome {
    ctx.shared().syscalls = ctx.shared().syscalls.saturating_add(1);
    if tracing() {
        eprintln!(
            "[wasmux {}] {}({:#x}, {:#x}, {:#x})",
            ctx.task.pid,
            crate::nr::name(number),
            args[0],
            args[1],
            args[2]
        );
    }
    match handle(ctx, s, number, args) {
        Ok(value) => {
            finish(ctx, s);
            Outcome::Value(value)
        }
        Err(Fault::Errno(e)) => {
            finish(ctx, s);
            Outcome::Value(e.as_neg())
        }
        Err(Fault::Block) => {
            // A signal that would be delivered interrupts the wait, unless every pending
            // handler asked for a restart.
            if let Some(()) = interrupted(ctx) {
                finish(ctx, s);
                return Outcome::Value(Errno::INTR.as_neg());
            }
            Outcome::Blocked
        }
        Err(Fault::Exit(status)) => Outcome::Exit(status),
        Err(Fault::Exec) => Outcome::Exec,
    }
}

/// A syscall is no longer outstanding: drop its deadline, put back a temporary signal mask,
/// and tell the guest whether a signal is waiting.
fn finish(ctx: &mut Ctx, s: &mut dyn Suspend) {
    ctx.exec.deadline = None;
    if let Some(mask) = ctx.exec.mask_to_restore.take() {
        ctx.task.sigmask = mask;
    }
    raise_signal_flag(ctx, s);
}

/// Tell musl that a signal is waiting, so it calls `sigfetch` on the way out of the syscall.
fn raise_signal_flag(ctx: &mut Ctx, s: &mut dyn Suspend) {
    let flag = ctx.exec.signal_flag;
    if flag == 0 {
        return;
    }
    let pending = ctx
        .shared()
        .record(ctx.task.pid)
        .map(|r| r.pending)
        .unwrap_or(0);
    let (live, _) = task::triage(pending, ctx.task.sigmask, &ctx.task);
    let mut m = memory(s);
    let _ = m.put_u32(flag, u32::from(live != 0));
}

/// Whether a blocked call should give up with `EINTR`.
///
/// Any deliverable signal ends the wait, `SA_RESTART` included. Linux would restart the call
/// itself after running the handler; nothing here can, because the handler runs in the guest
/// on the way out of the syscall. Returning `EINTR` is the honest half of that: the handler
/// gets to run, and a program that asked for a restart sees a documented error instead of
/// hanging. `sigsuspend` and `pause` are not restartable on Linux either, and they are exactly
/// the calls a shell uses to wait for a child.
fn interrupted(ctx: &mut Ctx) -> Option<()> {
    let pid = ctx.task.pid;
    let pending = ctx.shared().record(pid).map(|r| r.pending).unwrap_or(0);
    let (live, _) = task::triage(pending, ctx.task.sigmask, &ctx.task);
    if live == 0 {
        None
    } else {
        Some(())
    }
}

fn now_since_epoch() -> Duration {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
}

/// A deadline that survives being retried.
fn deadline(ctx: &mut Ctx, after: Duration) -> Instant {
    match ctx.exec.deadline {
        Some(when) => when,
        None => {
            let when = Instant::now()
                .checked_add(after)
                .unwrap_or_else(Instant::now);
            ctx.exec.deadline = Some(when);
            when
        }
    }
}

/// Resolve a path argument, honouring `dirfd` for the `*at` calls.
fn resolve(ctx: &mut Ctx, dirfd: i32, path: &str, follow: bool) -> Result<Node, Fault> {
    let base = if path.starts_with('/') {
        "/".to_string()
    } else if dirfd == AT_FDCWD {
        ctx.task.cwd.clone()
    } else {
        let entry = ctx.task.fds.get(dirfd)?;
        let sh = ctx.shared();
        let desc = sh.descs.get(entry.desc).ok_or(Errno::BADF)?;
        match desc.kind {
            DescKind::Dir { .. } => desc.path.clone(),
            _ => return Err(Errno::NOTDIR.into()),
        }
    };
    let who = ctx.who();
    let sh = ctx.shared();
    let resolver = sh.resolver();
    match resolver.resolve(&who, &base, path, follow) {
        Ok(node) => Ok(node),
        Err(e) => Err(vfs_fault(sh, e)),
    }
}

/// The default permission bits for a stat that did not supply any.
fn default_mode(kind: FileType) -> u32 {
    match kind {
        FileType::Dir => 0o755,
        FileType::File => 0o644,
        FileType::Symlink => 0o777,
    }
}

fn type_bits(kind: FileType) -> u32 {
    match kind {
        FileType::Dir => S_IFDIR,
        FileType::File => S_IFREG,
        FileType::Symlink => S_IFLNK,
    }
}

/// A stable inode number for a path that has none.
fn hashed_ino(path: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    (hasher.finish() >> 1) | 1
}

/// Fill in the `statx` buffer the guest passed.
fn write_statx(
    m: &mut Mem<'_>,
    at: u32,
    stat: &Stat,
    path: &str,
    rdev: (u32, u32),
) -> Result<(), Errno> {
    let mut buffer = [0u8; 256];
    let put32 = |b: &mut [u8; 256], off: usize, v: u32| {
        if let Some(slot) = b.get_mut(off..off.saturating_add(4)) {
            slot.copy_from_slice(&v.to_le_bytes());
        }
    };
    let put64 = |b: &mut [u8; 256], off: usize, v: u64| {
        if let Some(slot) = b.get_mut(off..off.saturating_add(8)) {
            slot.copy_from_slice(&v.to_le_bytes());
        }
    };
    let put16 = |b: &mut [u8; 256], off: usize, v: u16| {
        if let Some(slot) = b.get_mut(off..off.saturating_add(2)) {
            slot.copy_from_slice(&v.to_le_bytes());
        }
    };
    let mode = if stat.mode == 0 {
        default_mode(stat.file_type)
    } else {
        stat.mode
    };
    let ino = if stat.ino == 0 {
        hashed_ino(path)
    } else {
        stat.ino
    };
    put32(&mut buffer, 0, STATX_BASIC_STATS);
    put32(&mut buffer, 4, 4096);
    put32(&mut buffer, 16, stat.nlink.max(1));
    put16(&mut buffer, 28, (type_bits(stat.file_type) | mode) as u16);
    put64(&mut buffer, 32, ino);
    put64(&mut buffer, 40, stat.size);
    put64(&mut buffer, 48, stat.size.div_ceil(512));
    for offset in [64usize, 80, 96, 112] {
        put64(&mut buffer, offset, stat.mtime.0 as u64);
        put32(&mut buffer, offset.saturating_add(8), stat.mtime.1);
    }
    put32(&mut buffer, 128, rdev.0);
    put32(&mut buffer, 132, rdev.1);
    put32(&mut buffer, 136, 0);
    put32(&mut buffer, 140, 1);
    m.put(at, &buffer)
}

/// The content of a generated `/proc` file.
fn proc_content(ctx: &mut Ctx, guest: &str) -> Vec<u8> {
    let pid = ctx.task.pid;
    let comm = ctx.task.comm.clone();
    let argv = ctx.argv.clone();
    let sh = ctx.shared();
    let uptime = sh.started.elapsed();
    let name = guest.rsplit('/').next().unwrap_or("");
    let text = match name {
        "cpuinfo" => {
            "processor\t: 0\nvendor_id\t: wasmux\nmodel name\t: WebAssembly\nflags\t\t: wasm32\n"
                .to_string()
        }
        "meminfo" => {
            #[allow(clippy::integer_division, reason = "kB, truncated, as /proc does")]
            let (total, used) = (sh.limits.memory / 1024, sh.memory_used / 1024);
            format!(
                "MemTotal:       {total:8} kB\nMemFree:        {:8} kB\nMemAvailable:   {:8} kB\nBuffers:               0 kB\nCached:                0 kB\nSwapTotal:             0 kB\nSwapFree:              0 kB\n",
                total.saturating_sub(used),
                total.saturating_sub(used)
            )
        }
        "uptime" => format!("{:.2} {:.2}\n", uptime.as_secs_f64(), uptime.as_secs_f64()),
        "version" => format!("Linux version {KERNEL_RELEASE} (wasmux) #1 wasm32\n"),
        "loadavg" => "0.00 0.00 0.00 1/1 1\n".to_string(),
        "filesystems" => "nodev\tproc\n\twasmux\n".to_string(),
        "mounts" => {
            let mut out = String::new();
            for index in 0.. {
                match sh.mounts.get(index) {
                    Some(mount) => {
                        let flags = if mount.read_only { "ro" } else { "rw" };
                        out.push_str(&format!(
                            "wasmux {} wasmux {flags},relatime 0 0\n",
                            mount.at
                        ));
                    }
                    None => break,
                }
            }
            out.push_str("proc /proc proc rw,relatime 0 0\n");
            out
        }
        "stat" if guest == "/proc/stat" => "cpu  0 0 0 0 0 0 0 0 0 0\nprocesses 1\n".to_string(),
        "cmdline" => {
            let mut out = Vec::new();
            for arg in &argv {
                out.extend_from_slice(arg);
                out.push(0);
            }
            return out;
        }
        "comm" => format!("{comm}\n"),
        // /proc/<pid>/stat: the fields ps reads, and zeroes for the rest.
        "stat" => format!("{pid} ({comm}) R 1 {pid} {pid} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0\n"),
        "status" => format!("Name:\t{comm}\nPid:\t{pid}\nPPid:\t1\nThreads:\t1\n"),
        _ => String::new(),
    };
    text.into_bytes()
}

/// The block `_start` reads: `argc`, `argv`, `envp`, the auxiliary vector, and the random
/// bytes `AT_RANDOM` points at. Laid out at `base`, where the guest will put it.
pub(crate) fn start_block(ctx: &mut Ctx, base: u32) -> Vec<u8> {
    // argv, its NULL, envp, its NULL, and the argc slot.
    let pointer_count = ctx
        .argv
        .len()
        .saturating_add(ctx.envp.len())
        .saturating_add(3);
    let auxv: [(u32, u32); 10] = [
        (AT_PAGESZ, 4096),
        (AT_CLKTCK, 100),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_SECURE, 0),
        (AT_RANDOM, 0),
        (AT_EXECFN, 0),
        (AT_NULL, 0),
    ];
    let header = pointer_count
        .saturating_add(auxv.len().saturating_mul(2))
        .saturating_mul(4);
    let random_at = header;
    let strings_at = header.saturating_add(16);
    let mut strings: Vec<u8> = Vec::new();
    let mut offsets: Vec<u32> = Vec::new();
    for item in ctx.argv.iter().chain(ctx.envp.iter()) {
        offsets.push(strings_at.saturating_add(strings.len()) as u32);
        strings.extend_from_slice(item);
        strings.push(0);
    }
    let execfn_at = strings_at.saturating_add(strings.len()) as u32;
    strings.extend_from_slice(ctx.task.exe.as_bytes());
    strings.push(0);

    let mut out: Vec<u8> = Vec::with_capacity(strings_at.saturating_add(strings.len()));
    let mut push = |value: u32| out.extend_from_slice(&value.to_le_bytes());
    push(ctx.argv.len() as u32);
    let mut index = 0;
    for _ in &ctx.argv {
        push(base.wrapping_add(offsets.get(index).copied().unwrap_or(0)));
        index = index.saturating_add(1);
    }
    push(0);
    for _ in &ctx.envp {
        push(base.wrapping_add(offsets.get(index).copied().unwrap_or(0)));
        index = index.saturating_add(1);
    }
    push(0);
    for (key, value) in auxv {
        push(key);
        push(match key {
            AT_RANDOM => base.wrapping_add(random_at as u32),
            AT_EXECFN => base.wrapping_add(execfn_at),
            _ => value,
        });
    }
    let mut seed = [0u8; 16];
    fill_random(&mut seed);
    out.extend_from_slice(&seed);
    out.extend_from_slice(&strings);
    out
}

/// Pseudo-random bytes for `getrandom`, `AT_RANDOM` and `/dev/urandom`.
///
/// Deliberately not cryptographic: a sandbox with no network and no secrets does not need it,
/// and pulling in a source would mean asking the integrator for one.
fn fill_random(buffer: &mut [u8]) {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|state| {
        let mut x = state.get();
        if x == 0 {
            x = now_since_epoch().as_nanos() as u64 | 1;
        }
        for slot in buffer.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *slot = x as u8;
        }
        state.set(x);
    });
}

/// What `sigfetch` should tell the guest.
pub(crate) enum SignalDelivery {
    /// Nothing to do.
    None,
    /// The buffer has been filled; run the handler.
    Handler,
    /// The default action is to die, with this wait status.
    Fatal(i32),
}

/// Take the next deliverable signal, if any, and describe it to the guest.
pub(crate) fn next_signal(ctx: &mut Ctx, s: &mut dyn Suspend, out: u32) -> SignalDelivery {
    loop {
        let pid = ctx.task.pid;
        let pending = ctx.shared().record(pid).map(|r| r.pending).unwrap_or(0);
        let (live, discard) = task::triage(pending, ctx.task.sigmask, &ctx.task);
        if discard != 0 {
            if let Some(record) = ctx.shared().record_mut(pid) {
                record.pending &= !discard;
            }
        }
        if live == 0 {
            let flag = ctx.exec.signal_flag;
            if flag != 0 {
                let mut m = memory(s);
                let _ = m.put_u32(flag, 0);
            }
            return SignalDelivery::None;
        }
        let signal = live.trailing_zeros() as i32;
        if let Some(record) = ctx.shared().record_mut(pid) {
            record.pending &= !task::bit(signal);
        }
        let action = ctx.task.action(signal);
        if action.handler == SIG_IGN {
            continue;
        }
        if action.handler == SIG_DFL {
            if task::default_kills(signal) {
                return SignalDelivery::Fatal(task::status_signaled(signal));
            }
            continue;
        }
        if action.flags & SA_RESETHAND != 0 {
            ctx.task.set_action(signal, SigAction::default());
        }
        // The layout musl's delivery stub reads: signal, handler, flags, si_code, si_pid.
        let mut m = memory(s);
        let _ = m.put_u32(out, signal as u32);
        let _ = m.put_u32(out.wrapping_add(4), action.handler);
        let _ = m.put_u32(out.wrapping_add(8), action.flags);
        let _ = m.put_u32(out.wrapping_add(12), 0);
        let _ = m.put_u32(out.wrapping_add(16), 0);
        return SignalDelivery::Handler;
    }
}

// ---------------------------------------------------------------------------------------------
// the table

fn handle(ctx: &mut Ctx, s: &mut dyn Suspend, number: i32, a: [i32; 6]) -> Sys {
    let (a0, a1, a2, a3, a4) = (a[0], a[1], a[2], a[3], a[4]);
    let u = |v: i32| v as u32;
    match number {
        // ---- process lifetime ----
        nr::EXIT | nr::EXIT_GROUP => {
            let status = task::status_exited(a0);
            // A vfork child exiting hands the instance back to its parent.
            if let Some(parent) = ctx.vfork_stack.pop() {
                let child = core::mem::replace(&mut ctx.task, parent);
                let pid = child.pid;
                let sh = ctx.shared();
                sh.release_fds(child.fds);
                sh.set_exit(pid, status);
                return Ok(pid);
            }
            Err(Fault::Exit(status))
        }
        nr::CLONE => {
            let flags = u(a0);
            // Only vfork's shape is supported: share memory, parent waits for exec or exit.
            if flags & CLONE_VFORK == 0 || flags & CLONE_VM == 0 || flags & CLONE_THREAD != 0 {
                return Err(Errno::NOSYS.into());
            }
            let sh = ctx.shared();
            let pid = sh.fork_record(&ctx.task).ok_or(Errno::AGAIN)?;
            let fds = ctx.task.fds.duplicate(&mut sh.descs);
            let mut child = task::Task::new(pid, fds);
            child.cwd = ctx.task.cwd.clone();
            child.umask = ctx.task.umask;
            child.sigact = ctx.task.sigact;
            child.sigmask = ctx.task.sigmask;
            child.exe = ctx.task.exe.clone();
            child.comm = ctx.task.comm.clone();
            let parent = core::mem::replace(&mut ctx.task, child);
            ctx.vfork_stack.push(parent);
            Ok(0)
        }
        nr::EXECVE => {
            let m = memory(s);
            let path = m.cstr(u(a0))?;
            let argv = m.cstr_array(u(a1))?;
            let envp = m.cstr_array(u(a2))?;
            let node = resolve(ctx, AT_FDCWD, &path, true)?;
            let (program, exe) = match node {
                Node::Program { id, guest } => (id, guest),
                Node::Mounted { mount, rel, .. } => {
                    // A script: read the shebang and run its interpreter.
                    let sh = ctx.shared();
                    let vfs = sh.vfs(mount).ok_or(Errno::NOENT)?.clone();
                    // Linux reports `EACCES` for `execve` on a directory, not `EISDIR`,
                    // which is what opening one to look for a shebang would give.
                    if matches!(vfs.stat(&rel, true), Ok(st) if st.file_type == FileType::Dir) {
                        return Err(Errno::ACCES.into());
                    }
                    let mut file = vfs
                        .open(&rel, &OpenOptions::read())
                        .map_err(|e| vfs_fault(sh, e))?;
                    let mut head = [0u8; 256];
                    let read = file.read(&mut head).map_err(|e| vfs_fault(sh, e))?;
                    let _ = file.close();
                    let head = head.get(..read).unwrap_or(&[]);
                    let line = shebang(head).ok_or(Errno::NOEXEC)?;
                    let mut parts = line.splitn(2, char::is_whitespace);
                    let interpreter = parts.next().unwrap_or("").to_string();
                    let extra = parts
                        .next()
                        .map(str::trim)
                        .filter(|x| !x.is_empty())
                        .map(str::to_string);
                    let node = resolve(ctx, AT_FDCWD, &interpreter, true)?;
                    let (id, guest) = match node {
                        Node::Program { id, guest } => (id, guest),
                        _ => return Err(Errno::NOEXEC.into()),
                    };
                    let mut rebuilt: Vec<Vec<u8>> = vec![interpreter.into_bytes()];
                    if let Some(extra) = extra {
                        rebuilt.push(extra.into_bytes());
                    }
                    rebuilt.push(path.clone().into_bytes());
                    rebuilt.extend(argv.into_iter().skip(1));
                    return finish_exec(ctx, id, guest, rebuilt, envp);
                }
                _ => return Err(Errno::ACCES.into()),
            };
            finish_exec(ctx, program, exe, argv, envp)
        }
        nr::WAIT4 => {
            let (which, status_at, options, rusage_at) = (a0, u(a1), a2, u(a3));
            let pid = ctx.task.pid;
            let pgid = ctx.shared().record(pid).map(|r| r.pgid).unwrap_or(pid);
            let mut candidates: i32 = 0;
            let mut found = None;
            for record in ctx.shared().table.iter() {
                if record.ppid != pid {
                    continue;
                }
                let matches = match which {
                    -1 => true,
                    0 => record.pgid == pgid,
                    n if n > 0 => record.pid == n,
                    n => record.pgid == n.wrapping_neg(),
                };
                if !matches {
                    continue;
                }
                candidates = candidates.saturating_add(1);
                if let Some(status) = record.exit {
                    found = Some((record.pid, status));
                    break;
                }
            }
            if candidates == 0 {
                return Err(Errno::CHILD.into());
            }
            let Some((child, status)) = found else {
                if options & WNOHANG != 0 {
                    return Ok(0);
                }
                return Err(Fault::Block);
            };
            ctx.shared().table.retain(|r| r.pid != child);
            let mut m = memory(s);
            if status_at != 0 {
                m.put_u32(status_at, status as u32)?;
            }
            if rusage_at != 0 {
                m.zero(rusage_at, 72)?;
            }
            Ok(child)
        }
        nr::GETPID => Ok(ctx.task.pid),
        nr::GETTID => Ok(ctx.task.pid),
        nr::GETPPID => {
            let pid = ctx.task.pid;
            Ok(ctx.shared().record(pid).map(|r| r.ppid).unwrap_or(0))
        }
        nr::GETUID | nr::GETEUID | nr::GETGID | nr::GETEGID => Ok(0),
        nr::SETUID
        | nr::SETGID
        | nr::SETREUID
        | nr::SETREGID
        | nr::SETRESUID
        | nr::SETRESGID
        | nr::SETGROUPS => Ok(0),
        nr::GETGROUPS => Ok(0),
        nr::SETPGID => {
            let target = if a0 == 0 { ctx.task.pid } else { a0 };
            let group = if a1 == 0 { target } else { a1 };
            let record = ctx.shared().record_mut(target).ok_or(Errno::SRCH)?;
            record.pgid = group;
            Ok(0)
        }
        nr::GETPGID => {
            let target = if a0 == 0 { ctx.task.pid } else { a0 };
            Ok(ctx.shared().record(target).ok_or(Errno::SRCH)?.pgid)
        }
        nr::SETSID => {
            let pid = ctx.task.pid;
            let record = ctx.shared().record_mut(pid).ok_or(Errno::SRCH)?;
            record.pgid = pid;
            record.sid = pid;
            Ok(pid)
        }
        nr::GETSID => {
            let target = if a0 == 0 { ctx.task.pid } else { a0 };
            Ok(ctx.shared().record(target).ok_or(Errno::SRCH)?.sid)
        }
        nr::SET_TID_ADDRESS => Ok(ctx.task.pid),
        nr::SET_ROBUST_LIST | nr::PRCTL | nr::SCHED_YIELD | nr::MEMBARRIER | nr::RSEQ => Ok(0),
        nr::SCHED_GETAFFINITY => {
            if a2 >= 8 {
                memory(s).put_u64(u(a2), 1)?;
            }
            Ok(8)
        }
        nr::UMASK => {
            let previous = ctx.task.umask;
            ctx.task.umask = u(a0) & 0o777;
            Ok(previous as i32)
        }
        nr::UNAME => {
            let mut block = [0u8; 65 * 6];
            let fields = [
                "Linux",
                "wasmux",
                KERNEL_RELEASE,
                "#1 wasmux",
                "wasm32",
                "(none)",
            ];
            for (index, text) in fields.iter().enumerate() {
                let at = index.saturating_mul(65);
                if let Some(slot) = block.get_mut(at..at.saturating_add(text.len())) {
                    slot.copy_from_slice(text.as_bytes());
                }
            }
            memory(s).put(u(a0), &block)?;
            Ok(0)
        }
        nr::SYSINFO => {
            let sh = ctx.shared();
            let uptime = sh.started.elapsed().as_secs() as u32;
            let total = sh.limits.memory.min(u32::MAX as u64) as u32;
            let used = sh.memory_used.min(u64::from(total)) as u32;
            let processes = sh.table.iter().filter(|r| r.is_alive()).count() as u16;
            let mut block = [0u8; 64];
            let mut put32 = |off: usize, v: u32| {
                if let Some(slot) = block.get_mut(off..off.saturating_add(4)) {
                    slot.copy_from_slice(&v.to_le_bytes());
                }
            };
            put32(0, uptime);
            put32(16, total);
            put32(20, total.saturating_sub(used));
            put32(52, 1);
            if let Some(slot) = block.get_mut(40..42) {
                slot.copy_from_slice(&processes.to_le_bytes());
            }
            memory(s).put(u(a0), &block)?;
            Ok(0)
        }
        nr::PRLIMIT64 => {
            let (resource, old_at) = (a1, u(a3));
            if old_at != 0 {
                let sh = ctx.shared();
                let (soft, hard) = match resource {
                    RLIMIT_NOFILE => (sh.limits.open_files as u64, sh.limits.open_files as u64),
                    RLIMIT_STACK => (1 << 20, 1 << 20),
                    RLIMIT_CORE => (0, RLIM_INFINITY),
                    RLIMIT_DATA => (sh.limits.memory_per_process, sh.limits.memory_per_process),
                    _ => (RLIM_INFINITY, RLIM_INFINITY),
                };
                let mut m = memory(s);
                m.put_u64(old_at, soft)?;
                m.put_u64(old_at.wrapping_add(8), hard)?;
            }
            Ok(0)
        }
        nr::GETRUSAGE => {
            memory(s).zero(u(a1), 72)?;
            Ok(0)
        }
        nr::TIMES => {
            if a0 != 0 {
                memory(s).zero(u(a0), 16)?;
            }
            #[allow(clippy::integer_division, reason = "clock ticks are centiseconds")]
            let ticks = ctx.shared().started.elapsed().as_millis() / 10;
            Ok(ticks as i32)
        }
        nr::GETRANDOM => {
            let mut m = memory(s);
            let buffer = m.bytes_mut(u(a0), u(a1))?;
            fill_random(buffer);
            Ok(a1)
        }

        // ---- signals ----
        nr::RT_SIGACTION => {
            let (signal, action_at, old_at) = (a0, u(a1), u(a2));
            if signal <= 0 || signal as usize >= NSIG {
                return Err(Errno::INVAL.into());
            }
            let mut m = memory(s);
            if old_at != 0 {
                let previous = ctx.task.action(signal);
                m.put_u32(old_at, previous.handler)?;
                m.put_u32(old_at.wrapping_add(4), previous.flags)?;
                m.put_u64(old_at.wrapping_add(8), previous.mask)?;
                m.put_u32(old_at.wrapping_add(16), 0)?;
            }
            if action_at != 0 {
                if signal == SIGKILL || signal == SIGSTOP {
                    return Err(Errno::INVAL.into());
                }
                let handler = m.u32(action_at)?;
                let flags = m.u32(action_at.wrapping_add(4))?;
                let mask = m.u64(action_at.wrapping_add(8))?;
                ctx.task.set_action(
                    signal,
                    SigAction {
                        handler,
                        flags,
                        mask,
                    },
                );
            }
            Ok(0)
        }
        nr::RT_SIGPROCMASK => {
            let (how, set_at, old_at) = (a0, u(a1), u(a2));
            let previous = ctx.task.sigmask;
            let mut m = memory(s);
            if set_at != 0 {
                let set = m.u64(set_at)? & !(task::bit(SIGKILL) | task::bit(SIGSTOP));
                ctx.task.sigmask = match how {
                    SIG_BLOCK => previous | set,
                    SIG_UNBLOCK => previous & !set,
                    SIG_SETMASK => set,
                    _ => return Err(Errno::INVAL.into()),
                };
            }
            if old_at != 0 {
                m.put_u64(old_at, previous)?;
            }
            Ok(0)
        }
        nr::RT_SIGPENDING => {
            let pid = ctx.task.pid;
            let pending = ctx.shared().record(pid).map(|r| r.pending).unwrap_or(0);
            memory(s).put_u64(u(a0), pending)?;
            Ok(0)
        }
        nr::RT_SIGSUSPEND => {
            let mask = memory(s).u64(u(a0))?;
            // Install the temporary mask once, and leave it until the call returns: it is what
            // decides which signal is allowed to wake this process.
            if ctx.exec.mask_to_restore.is_none() {
                ctx.exec.mask_to_restore = Some(ctx.task.sigmask);
            }
            ctx.task.sigmask = mask;
            if interrupted(ctx).is_some() {
                Err(Errno::INTR.into())
            } else {
                Err(Fault::Block)
            }
        }
        nr::SIGALTSTACK | nr::RT_SIGRETURN => Ok(0),
        nr::KILL => {
            send_signal(ctx, a0, a1)?;
            Ok(0)
        }
        nr::TKILL => {
            send_signal(ctx, a0, a1)?;
            Ok(0)
        }
        nr::TGKILL => {
            send_signal(ctx, a1, a2)?;
            Ok(0)
        }

        // ---- time ----
        nr::CLOCK_GETTIME64 => {
            let value = match a0 {
                CLOCK_REALTIME => now_since_epoch(),
                CLOCK_MONOTONIC | CLOCK_BOOTTIME => ctx.shared().started.elapsed(),
                CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => {
                    ctx.shared().started.elapsed()
                }
                _ => return Err(Errno::INVAL.into()),
            };
            memory(s).put_timespec(u(a1), value)?;
            Ok(0)
        }
        nr::CLOCK_GETRES_TIME64 => {
            if a1 != 0 {
                memory(s).put_timespec(u(a1), Duration::from_nanos(1))?;
            }
            Ok(0)
        }
        nr::GETTIMEOFDAY => {
            let now = now_since_epoch();
            let mut m = memory(s);
            m.put_u32(u(a0), now.as_secs() as u32)?;
            m.put_u32(u(a0).wrapping_add(4), now.subsec_micros())?;
            Ok(0)
        }
        nr::NANOSLEEP | nr::CLOCK_NANOSLEEP_TIME64 => {
            let (request_at, absolute) = if number == nr::NANOSLEEP {
                (u(a0), false)
            } else {
                (u(a2), a1 & TIMER_ABSTIME != 0)
            };
            let requested = memory(s).timespec(request_at)?;
            let until = if absolute {
                let now = if a0 == CLOCK_REALTIME {
                    now_since_epoch()
                } else {
                    ctx.shared().started.elapsed()
                };
                deadline(ctx, requested.saturating_sub(now))
            } else {
                deadline(ctx, requested)
            };
            if Instant::now() >= until {
                return Ok(0);
            }
            let pid = ctx.task.pid;
            if let Some(record) = ctx.shared().record_mut(pid) {
                record.wake_at = Some(until);
            }
            Err(Fault::Block)
        }

        // ---- memory ----
        nr::BRK => sys_brk(ctx, s, u(a0)),
        nr::MMAP => sys_mmap(ctx, s, a),
        nr::MUNMAP | nr::MPROTECT | nr::MADVISE | nr::MSYNC | nr::MLOCK | nr::MUNLOCK => Ok(0),
        nr::MREMAP => Err(Errno::NOMEM.into()),

        // ---- files ----
        nr::OPENAT => sys_openat(ctx, s, a0, u(a1), u(a2), u(a3)),
        nr::CLOSE => {
            let entry = ctx.task.fds.take(a0)?;
            ctx.shared().release_desc(entry.desc);
            Ok(0)
        }
        nr::READ => sys_read(ctx, s, a0, u(a1), u(a2)),
        nr::WRITE => sys_write(ctx, s, a0, u(a1), u(a2)),
        nr::READV | nr::WRITEV => {
            let (fd, vector, count) = (a0, u(a1), a2.max(0) as u32);
            let mut total = 0i32;
            for index in 0..count {
                let entry = vector.wrapping_add(index.wrapping_mul(8));
                let m = memory(s);
                let at = m.u32(entry)?;
                let len = m.u32(entry.wrapping_add(4))?;
                if len == 0 {
                    continue;
                }
                let result = if number == nr::READV {
                    sys_read(ctx, s, fd, at, len)
                } else {
                    sys_write(ctx, s, fd, at, len)
                };
                match result {
                    Ok(done) => {
                        total = total.saturating_add(done);
                        if (done as u32) < len {
                            break;
                        }
                    }
                    Err(fault) => {
                        if total > 0 {
                            break;
                        }
                        return Err(fault);
                    }
                }
            }
            Ok(total)
        }
        nr::LSEEK => {
            let offset = a1 as i64;
            let entry = ctx.task.fds.get(a0)?;
            let sh = ctx.shared();
            let desc = sh.descs.get_mut(entry.desc).ok_or(Errno::BADF)?;
            let position = match &mut desc.kind {
                DescKind::File { file, .. } => {
                    let from = match a2 {
                        SEEK_SET => SeekFrom::Start(offset.max(0) as u64),
                        SEEK_CUR => SeekFrom::Current(offset),
                        SEEK_END => SeekFrom::End(offset),
                        _ => return Err(Errno::INVAL.into()),
                    };
                    match file.seek(from) {
                        Ok(position) => position,
                        Err(e) => return Err(vfs_fault(sh, e)),
                    }
                }
                DescKind::Dir { pos, .. } => {
                    if a2 == SEEK_SET && offset == 0 {
                        *pos = 0;
                    }
                    0
                }
                DescKind::Null | DescKind::Zero | DescKind::Random => 0,
                _ => return Err(Errno::SPIPE.into()),
            };
            if position > i32::MAX as u64 {
                return Err(Errno::OVERFLOW.into());
            }
            Ok(position as i32)
        }
        nr::FTRUNCATE => {
            let length = u64::from(u(a1)) | (u64::from(u(a2)) << 32);
            let entry = ctx.task.fds.get(a0)?;
            let sh = ctx.shared();
            let mount = match sh.descs.get(entry.desc).map(|d| &d.kind) {
                Some(DescKind::File { mount, .. }) => *mount,
                Some(_) => return Err(Errno::INVAL.into()),
                None => return Err(Errno::BADF.into()),
            };
            if sh.is_read_only(mount) {
                return Err(Errno::ROFS.into());
            }
            let Some(Desc {
                kind: DescKind::File { file, .. },
                ..
            }) = sh.descs.get_mut(entry.desc)
            else {
                return Err(Errno::BADF.into());
            };
            match file.set_len(length) {
                Ok(()) => Ok(0),
                Err(e) => Err(vfs_fault(sh, e)),
            }
        }
        nr::TRUNCATE => {
            let path = memory(s).cstr(u(a0))?;
            let length = u64::from(u(a1)) | (u64::from(u(a2)) << 32);
            match resolve(ctx, AT_FDCWD, &path, true)? {
                Node::Mounted { mount, rel, .. } => {
                    let sh = ctx.shared();
                    if sh.is_read_only(mount) {
                        return Err(Errno::ROFS.into());
                    }
                    let vfs = sh.vfs(mount).ok_or(Errno::NOENT)?.clone();
                    vfs.truncate(&rel, length).map_err(|e| vfs_fault(sh, e))?;
                    Ok(0)
                }
                _ => Err(Errno::ROFS.into()),
            }
        }
        nr::FSYNC | nr::FDATASYNC | nr::SYNC | nr::SYNCFS | nr::FADVISE64 | nr::FLOCK => Ok(0),
        nr::DUP => {
            let entry = ctx.task.fds.get(a0)?;
            let sh = ctx.shared();
            if let Some(desc) = sh.descs.get_mut(entry.desc) {
                desc.refs = desc.refs.saturating_add(1);
            }
            let limit = sh.limits.open_files;
            ctx.task
                .fds
                .alloc(
                    Fd {
                        desc: entry.desc,
                        cloexec: false,
                    },
                    0,
                    limit,
                )
                .map_err(Fault::from)
        }
        nr::DUP3 => {
            if a0 == a1 {
                return Err(Errno::INVAL.into());
            }
            let entry = ctx.task.fds.get(a0)?;
            let sh = ctx.shared();
            if let Some(desc) = sh.descs.get_mut(entry.desc) {
                desc.refs = desc.refs.saturating_add(1);
            }
            let limit = sh.limits.open_files;
            let replaced = ctx.task.fds.replace(
                a1,
                Fd {
                    desc: entry.desc,
                    cloexec: u(a2) & O_CLOEXEC != 0,
                },
                limit,
            )?;
            if let Some(old) = replaced {
                ctx.shared().release_desc(old.desc);
            }
            Ok(a1)
        }
        nr::PIPE2 => {
            let flags = u(a1);
            let sh = ctx.shared();
            let limit = sh.limits.open_files;
            let pipe = sh.pipes.insert(Pipe::new(), limit).ok_or(Errno::MFILE)?;
            let read_desc = sh
                .descs
                .insert(
                    Desc::new(
                        DescKind::PipeRead(pipe),
                        "pipe:",
                        O_RDONLY | (flags & O_NONBLOCK),
                    ),
                    limit.saturating_mul(4),
                )
                .ok_or(Errno::MFILE)?;
            let write_desc = sh
                .descs
                .insert(
                    Desc::new(
                        DescKind::PipeWrite(pipe),
                        "pipe:",
                        O_WRONLY | (flags & O_NONBLOCK),
                    ),
                    limit.saturating_mul(4),
                )
                .ok_or(Errno::MFILE)?;
            let cloexec = flags & O_CLOEXEC != 0;
            let read_fd = ctx.task.fds.alloc(
                Fd {
                    desc: read_desc,
                    cloexec,
                },
                0,
                limit,
            )?;
            let write_fd = match ctx.task.fds.alloc(
                Fd {
                    desc: write_desc,
                    cloexec,
                },
                0,
                limit,
            ) {
                Ok(fd) => fd,
                Err(e) => {
                    if let Ok(entry) = ctx.task.fds.take(read_fd) {
                        ctx.shared().release_desc(entry.desc);
                    }
                    return Err(e.into());
                }
            };
            let mut m = memory(s);
            m.put_u32(u(a0), read_fd as u32)?;
            m.put_u32(u(a0).wrapping_add(4), write_fd as u32)?;
            Ok(0)
        }
        nr::FCNTL => {
            let (fd, command, argument) = (a0, a1, a2);
            match command {
                F_DUPFD | F_DUPFD_CLOEXEC => {
                    let entry = ctx.task.fds.get(fd)?;
                    let sh = ctx.shared();
                    if let Some(desc) = sh.descs.get_mut(entry.desc) {
                        desc.refs = desc.refs.saturating_add(1);
                    }
                    let limit = sh.limits.open_files;
                    let cloexec = command == F_DUPFD_CLOEXEC;
                    ctx.task
                        .fds
                        .alloc(
                            Fd {
                                desc: entry.desc,
                                cloexec,
                            },
                            argument.max(0) as usize,
                            limit,
                        )
                        .map_err(Fault::from)
                }
                F_GETFD => Ok(i32::from(ctx.task.fds.get(fd)?.cloexec) * FD_CLOEXEC),
                F_SETFD => {
                    ctx.task.fds.set_cloexec(fd, argument & FD_CLOEXEC != 0)?;
                    Ok(0)
                }
                F_GETFL => {
                    let entry = ctx.task.fds.get(fd)?;
                    Ok(ctx.shared().descs.get(entry.desc).ok_or(Errno::BADF)?.flags as i32)
                }
                F_SETFL => {
                    let entry = ctx.task.fds.get(fd)?;
                    let desc = ctx.shared().descs.get_mut(entry.desc).ok_or(Errno::BADF)?;
                    desc.flags = (desc.flags & O_ACCMODE) | (u(argument) & (O_APPEND | O_NONBLOCK));
                    Ok(0)
                }
                F_GETLK | F_SETLK | F_SETLKW => {
                    ctx.task.fds.get(fd)?;
                    Ok(0)
                }
                _ => Err(Errno::INVAL.into()),
            }
        }
        nr::IOCTL => sys_ioctl(ctx, s, a0, u(a1), u(a2)),
        nr::GETDENTS64 => {
            let entry = ctx.task.fds.get(a0)?;
            let (at, capacity) = (u(a1), u(a2));
            let sh = ctx.shared();
            let desc = sh.descs.get_mut(entry.desc).ok_or(Errno::BADF)?;
            let DescKind::Dir { entries, pos } = &mut desc.kind else {
                return Err(Errno::NOTDIR.into());
            };
            // Build the records first so guest memory is touched once.
            let mut block: Vec<u8> = Vec::new();
            while let Some(item) = entries.get(*pos) {
                let record_len = 19usize
                    .saturating_add(item.name.len())
                    .saturating_add(1)
                    .next_multiple_of(8);
                if block.len().saturating_add(record_len) > capacity as usize {
                    if block.is_empty() {
                        return Err(Errno::INVAL.into());
                    }
                    break;
                }
                let offset = pos.saturating_add(1) as u64;
                block.extend_from_slice(&item.ino.to_le_bytes());
                block.extend_from_slice(&offset.to_le_bytes());
                block.extend_from_slice(&(record_len as u16).to_le_bytes());
                block.push(item.kind);
                block.extend_from_slice(item.name.as_bytes());
                block.resize(
                    block
                        .len()
                        .saturating_add(record_len)
                        .saturating_sub(item.name.len().saturating_add(19)),
                    0,
                );
                *pos = pos.saturating_add(1);
            }
            let length = block.len();
            memory(s).put(at, &block)?;
            Ok(length as i32)
        }
        nr::GETCWD => {
            let cwd = ctx.task.cwd.clone();
            let bytes = cwd.as_bytes();
            if (u(a1) as usize) < bytes.len().saturating_add(1) {
                return Err(Errno::RANGE.into());
            }
            let mut m = memory(s);
            m.put(u(a0), bytes)?;
            m.put_u8(u(a0).wrapping_add(bytes.len() as u32), 0)?;
            Ok((bytes.len() as i32).saturating_add(1))
        }
        nr::CHDIR => {
            let path = memory(s).cstr(u(a0))?;
            let node = resolve(ctx, AT_FDCWD, &path, true)?;
            let target = match &node {
                Node::VirtualDir { guest } => guest.clone(),
                Node::Mounted { mount, rel, guest } => {
                    let (mount, rel, guest) = (*mount, rel.clone(), guest.clone());
                    let sh = ctx.shared();
                    let vfs = sh.vfs(mount).ok_or(Errno::NOENT)?.clone();
                    match vfs.stat(&rel, true) {
                        Ok(stat) if stat.file_type == FileType::Dir => guest,
                        Ok(_) => return Err(Errno::NOTDIR.into()),
                        Err(e) => return Err(vfs_fault(sh, e)),
                    }
                }
                _ => return Err(Errno::NOTDIR.into()),
            };
            ctx.task.cwd = target;
            Ok(0)
        }
        nr::FCHDIR => {
            let entry = ctx.task.fds.get(a0)?;
            let sh = ctx.shared();
            let desc = sh.descs.get(entry.desc).ok_or(Errno::BADF)?;
            match desc.kind {
                DescKind::Dir { .. } => {
                    let path = desc.path.clone();
                    ctx.task.cwd = path;
                    Ok(0)
                }
                _ => Err(Errno::NOTDIR.into()),
            }
        }
        nr::STATX => sys_statx(ctx, s, a0, u(a1), a2, u(a4)),
        // musl on this target reaches for statx first and only falls back to these.
        nr::NEWFSTATAT | nr::FSTAT => Err(Errno::NOSYS.into()),
        nr::FACCESSAT | nr::FACCESSAT2 => {
            let path = memory(s).cstr(u(a1))?;
            let follow = a3 & AT_SYMLINK_NOFOLLOW == 0;
            match resolve(ctx, a0, &path, follow)? {
                Node::Mounted { mount, rel, .. } => {
                    let sh = ctx.shared();
                    let vfs = sh.vfs(mount).ok_or(Errno::NOENT)?.clone();
                    match vfs.stat(&rel, follow) {
                        Ok(_) => Ok(0),
                        Err(e) => Err(vfs_fault(sh, e)),
                    }
                }
                _ => Ok(0),
            }
        }
        nr::READLINKAT => {
            let path = memory(s).cstr(u(a1))?;
            let target = match resolve(ctx, a0, &path, false)? {
                Node::VirtualLink { target } => target,
                Node::Device(Device::Fd(fd)) => {
                    let entry = ctx.task.fds.get(fd)?;
                    ctx.shared()
                        .descs
                        .get(entry.desc)
                        .ok_or(Errno::BADF)?
                        .path
                        .clone()
                }
                Node::Mounted { mount, rel, .. } => {
                    let sh = ctx.shared();
                    let vfs = sh.vfs(mount).ok_or(Errno::NOENT)?.clone();
                    vfs.readlink(&rel).map_err(|e| vfs_fault(sh, e))?
                }
                _ => return Err(Errno::INVAL.into()),
            };
            let bytes = target.as_bytes();
            let take = bytes.len().min(u(a3) as usize);
            memory(s).put(u(a2), bytes.get(..take).unwrap_or(&[]))?;
            Ok(take as i32)
        }
        nr::MKDIRAT => {
            let path = memory(s).cstr(u(a1))?;
            let mode = u(a2) & !ctx.task.umask & 0o777;
            match resolve(ctx, a0, &path, true) {
                Ok(Node::Mounted { mount, rel, .. }) => {
                    let sh = ctx.shared();
                    if sh.is_read_only(mount) {
                        return Err(Errno::ROFS.into());
                    }
                    let vfs = sh.vfs(mount).ok_or(Errno::NOENT)?.clone();
                    vfs.mkdir(&rel, mode).map_err(|e| vfs_fault(sh, e))?;
                    Ok(0)
                }
                Ok(_) => Err(Errno::EXIST.into()),
                Err(Fault::Errno(Errno::NOENT)) => Err(Errno::ROFS.into()),
                Err(other) => Err(other),
            }
        }
        nr::UNLINKAT => {
            let path = memory(s).cstr(u(a1))?;
            let remove_dir = a2 & AT_REMOVEDIR != 0;
            match resolve(ctx, a0, &path, false)? {
                Node::Mounted { mount, rel, .. } => {
                    let sh = ctx.shared();
                    if sh.is_read_only(mount) {
                        return Err(Errno::ROFS.into());
                    }
                    let vfs = sh.vfs(mount).ok_or(Errno::NOENT)?.clone();
                    let result = if remove_dir {
                        vfs.rmdir(&rel)
                    } else {
                        vfs.unlink(&rel)
                    };
                    result.map_err(|e| vfs_fault(sh, e))?;
                    Ok(0)
                }
                _ => Err(Errno::ROFS.into()),
            }
        }
        nr::RENAMEAT | nr::RENAMEAT2 => {
            let (from_path, to_path) = {
                let m = memory(s);
                (m.cstr(u(a1))?, m.cstr(u(a3))?)
            };
            let from = resolve(ctx, a0, &from_path, false)?;
            let to = resolve(ctx, a2, &to_path, false)?;
            match (from, to) {
                (
                    Node::Mounted {
                        mount: m1, rel: r1, ..
                    },
                    Node::Mounted {
                        mount: m2, rel: r2, ..
                    },
                ) => {
                    if m1 != m2 {
                        return Err(Errno::XDEV.into());
                    }
                    let sh = ctx.shared();
                    if sh.is_read_only(m1) {
                        return Err(Errno::ROFS.into());
                    }
                    let vfs = sh.vfs(m1).ok_or(Errno::NOENT)?.clone();
                    vfs.rename(&r1, &r2).map_err(|e| vfs_fault(sh, e))?;
                    Ok(0)
                }
                _ => Err(Errno::ROFS.into()),
            }
        }
        nr::SYMLINKAT => {
            let (target, link_path) = {
                let m = memory(s);
                (m.cstr(u(a0))?, m.cstr(u(a2))?)
            };
            match resolve(ctx, a1, &link_path, false) {
                Ok(Node::Mounted { mount, rel, .. }) => {
                    let sh = ctx.shared();
                    if sh.is_read_only(mount) {
                        return Err(Errno::ROFS.into());
                    }
                    let vfs = sh.vfs(mount).ok_or(Errno::NOENT)?.clone();
                    vfs.symlink(&target, &rel).map_err(|e| vfs_fault(sh, e))?;
                    Ok(0)
                }
                Ok(_) => Err(Errno::EXIST.into()),
                Err(Fault::Errno(Errno::NOENT)) => Err(Errno::ROFS.into()),
                Err(other) => Err(other),
            }
        }
        nr::LINKAT => Err(Errno::PERM.into()),
        nr::FCHMODAT => {
            let path = memory(s).cstr(u(a1))?;
            match resolve(ctx, a0, &path, true)? {
                Node::Mounted { mount, rel, .. } => {
                    let sh = ctx.shared();
                    if sh.is_read_only(mount) {
                        return Err(Errno::ROFS.into());
                    }
                    let vfs = sh.vfs(mount).ok_or(Errno::NOENT)?.clone();
                    vfs.set_mode(&rel, u(a2) & 0o7777)
                        .map_err(|e| vfs_fault(sh, e))?;
                    Ok(0)
                }
                _ => Ok(0),
            }
        }
        nr::FCHMOD | nr::FCHOWN | nr::FCHOWNAT => Ok(0),
        // Timestamps are not stored, but the path still has to exist: `touch` sets the time
        // first and only creates the file when that fails, so accepting everything here means
        // `touch` silently creates nothing.
        nr::UTIMENSAT_TIME64 => {
            let path = memory(s).cstr(u(a1))?;
            if path.is_empty() {
                return Ok(0);
            }
            match resolve(ctx, a0, &path, a4 & AT_SYMLINK_NOFOLLOW == 0)? {
                Node::Mounted { mount, rel, .. } => {
                    let sh = ctx.shared();
                    let vfs = sh.vfs(mount).ok_or(Errno::NOENT)?.clone();
                    match vfs.stat(&rel, true) {
                        Ok(_) => Ok(0),
                        Err(e) => Err(vfs_fault(sh, e)),
                    }
                }
                _ => Ok(0),
            }
        }
        nr::STATFS | nr::FSTATFS => {
            let mut block = [0u8; 88];
            let mut put32 = |off: usize, v: u32| {
                if let Some(slot) = block.get_mut(off..off.saturating_add(4)) {
                    slot.copy_from_slice(&v.to_le_bytes());
                }
            };
            put32(0, 0x5741534d); // "WASM"
            put32(4, 4096);
            put32(56, 255);
            put32(60, 4096);
            memory(s).put(u(a2), &block)?;
            Ok(0)
        }

        // ---- waiting on descriptors ----
        nr::PPOLL_TIME64 => sys_ppoll(ctx, s, u(a0), u(a1), u(a2)),
        nr::PSELECT6_TIME64 => sys_pselect(ctx, s, a0, u(a1), u(a2), u(a3), u(a4)),

        // ---- deliberately absent ----
        nr::SOCKET
        | nr::SOCKETPAIR
        | nr::CONNECT
        | nr::BIND
        | nr::LISTEN
        | nr::ACCEPT
        | nr::ACCEPT4
        | nr::SENDTO
        | nr::RECVFROM
        | nr::SENDMSG
        | nr::RECVMSG
        | nr::SHUTDOWN
        | nr::GETSOCKNAME
        | nr::GETPEERNAME
        | nr::SETSOCKOPT
        | nr::GETSOCKOPT => Err(Errno::NOSYS.into()),

        _ => Err(Errno::NOSYS.into()),
    }
}

/// Kernel release string, reported by `uname` and `/proc/version`.
const KERNEL_RELEASE: &str = "6.1.0-wasmux";

fn shebang(head: &[u8]) -> Option<String> {
    let rest = head.strip_prefix(b"#!")?;
    let line = rest.split(|&b| b == b'\n').next().unwrap_or(&[]);
    let text = String::from_utf8_lossy(line).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Finish an `execve`: either hand the child to the scheduler, or replace this instance.
fn finish_exec(
    ctx: &mut Ctx,
    program: usize,
    exe: String,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> Sys {
    // Close-on-exec applies whichever way this goes.
    let closing = ctx.task.fds.drain_cloexec();
    let sh = ctx.shared();
    for key in closing {
        sh.release_desc(key);
    }
    if let Some(parent) = ctx.vfork_stack.pop() {
        // A vfork child: the parent's instance is restored here and the child becomes a
        // process of its own, which is what makes vfork return the child's pid.
        let child = core::mem::replace(&mut ctx.task, parent);
        let pid = child.pid;
        ctx.shared().spawn.push(Spawn {
            task: child,
            program,
            exe,
            argv,
            envp,
        });
        return Ok(pid);
    }
    let task = core::mem::replace(&mut ctx.task, task::Task::new(0, Default::default()));
    ctx.exec_request = Some(Spawn {
        task,
        program,
        exe,
        argv,
        envp,
    });
    Err(Fault::Exec)
}

fn send_signal(ctx: &mut Ctx, target: i32, signal: i32) -> Result<(), Fault> {
    if !(0..NSIG as i32).contains(&signal) {
        return Err(Errno::INVAL.into());
    }
    let me = ctx.task.pid;
    let my_group = ctx.shared().record(me).map(|r| r.pgid).unwrap_or(me);
    let sh = ctx.shared();
    let mut delivered = false;
    for record in sh.table.iter_mut() {
        let matches = if target > 0 {
            record.pid == target
        } else if target == 0 {
            record.pgid == my_group && record.is_alive()
        } else if target == -1 {
            record.pid != 1 && record.pid != me && record.is_alive()
        } else {
            record.pgid == target.wrapping_neg() && record.is_alive()
        };
        if matches {
            delivered = true;
            if signal != 0 {
                record.raise(signal);
                record.wake_at = None;
            }
        }
    }
    if delivered {
        Ok(())
    } else {
        Err(Errno::SRCH.into())
    }
}

fn sys_brk(ctx: &mut Ctx, s: &mut dyn Suspend, request: u32) -> Sys {
    let (base, _) = s.mem();
    let _ = base;
    let size = {
        let (_, len) = s.mem();
        len as u64
    };
    let state = &mut ctx.exec;
    let _ = state;
    // The break starts at the end of the initial memory and only ever grows it.
    let current = size;
    if request == 0 || u64::from(request) <= current {
        return Ok(current as i32);
    }
    let wanted = u64::from(request).saturating_sub(current);
    let pages = wanted.div_ceil(super::PAGE);
    let per_process = ctx.shared().limits.memory_per_process;
    if u64::from(request) > per_process {
        return Ok(current as i32);
    }
    let bytes = pages.saturating_mul(super::PAGE);
    if !ctx.shared().charge_memory(bytes) {
        return Ok(current as i32);
    }
    match s.mem_grow(pages) {
        Some(_) => Ok(request as i32),
        None => {
            ctx.shared().release_memory(bytes);
            Ok(current as i32)
        }
    }
}

fn sys_mmap(ctx: &mut Ctx, s: &mut dyn Suspend, a: [i32; 6]) -> Sys {
    let (addr, length, flags, fd, offset) = (a[0] as u32, a[1] as u32, a[3], a[4], a[5] as u64);
    if length == 0 {
        return Err(Errno::INVAL.into());
    }
    if flags & MAP_FIXED != 0 && addr != 0 {
        return Err(Errno::NOMEM.into());
    }
    let pages = u64::from(length).div_ceil(super::PAGE);
    let bytes = pages.saturating_mul(super::PAGE);
    let per_process = ctx.shared().limits.memory_per_process;
    let current = {
        let (_, len) = s.mem();
        len as u64
    };
    if current.saturating_add(bytes) > per_process {
        return Err(Errno::NOMEM.into());
    }
    if !ctx.shared().charge_memory(bytes) {
        return Err(Errno::NOMEM.into());
    }
    let Some(previous) = s.mem_grow(pages) else {
        ctx.shared().release_memory(bytes);
        return Err(Errno::NOMEM.into());
    };
    let at = previous.saturating_mul(super::PAGE) as u32;
    if flags & MAP_ANONYMOUS == 0 {
        // A file mapping is a read, once: there is no page fault to make it lazy.
        let entry = ctx.task.fds.get(fd)?;
        let sh = ctx.shared();
        let desc = sh.descs.get_mut(entry.desc).ok_or(Errno::BADF)?;
        let DescKind::File { file, .. } = &mut desc.kind else {
            return Err(Errno::NODEV.into());
        };
        let mut buffer = vec![0u8; length as usize];
        let outcome: Result<(), Errno> = (|| {
            let saved = file.seek(SeekFrom::Current(0))?;
            file.seek(SeekFrom::Start(offset.saturating_mul(4096)))?;
            let mut filled = 0usize;
            while filled < buffer.len() {
                let Some(window) = buffer.get_mut(filled..) else {
                    break;
                };
                match file.read(window) {
                    Ok(0) => break,
                    Ok(n) => filled = filled.saturating_add(n),
                    Err(e) => return Err(e),
                }
            }
            let _ = file.seek(SeekFrom::Start(saved));
            Ok(())
        })();
        if let Err(e) = outcome {
            return Err(vfs_fault(sh, e));
        }
        // Memory was grown above, so take a fresh view of it.
        memory(s).put(at, &buffer)?;
    }
    Ok(at as i32)
}

fn sys_openat(
    ctx: &mut Ctx,
    s: &mut dyn Suspend,
    dirfd: i32,
    path_at: u32,
    flags: u32,
    mode: u32,
) -> Sys {
    let path = memory(s).cstr(path_at)?;
    let follow = flags & O_NOFOLLOW == 0;
    let node = resolve(ctx, dirfd, &path, follow)?;
    let cloexec = flags & O_CLOEXEC != 0;
    let status = flags & (O_ACCMODE | O_APPEND | O_NONBLOCK);
    let access = flags & O_ACCMODE;
    let wants_write = access == O_WRONLY || access == O_RDWR || flags & (O_CREAT | O_TRUNC) != 0;

    let (kind, guest) = match node {
        Node::Program { guest, .. } => {
            if wants_write {
                return Err(Errno::ROFS.into());
            }
            // A program is executable but not readable: nothing can copy it out.
            (DescKind::Null, guest)
        }
        Node::Device(device) => {
            let kind = match device {
                Device::Null => DescKind::Null,
                Device::Zero => DescKind::Zero,
                Device::Random => DescKind::Random,
                Device::Tty => {
                    if access == O_RDONLY {
                        DescKind::Stdin
                    } else {
                        DescKind::Stdout(1)
                    }
                }
                Device::Fd(fd) => {
                    // Reopening an existing descriptor: share the description.
                    let entry = ctx.task.fds.get(fd)?;
                    let sh = ctx.shared();
                    if let Some(desc) = sh.descs.get_mut(entry.desc) {
                        desc.refs = desc.refs.saturating_add(1);
                    }
                    let limit = sh.limits.open_files;
                    return ctx
                        .task
                        .fds
                        .alloc(
                            Fd {
                                desc: entry.desc,
                                cloexec,
                            },
                            0,
                            limit,
                        )
                        .map_err(Fault::from);
                }
            };
            (kind, String::new())
        }
        Node::Synth { guest } => {
            if wants_write {
                return Err(Errno::ROFS.into());
            }
            let content = proc_content(ctx, &guest);
            (
                DescKind::Memory {
                    data: content,
                    pos: 0,
                },
                guest,
            )
        }
        Node::VirtualDir { guest } => {
            if wants_write {
                return Err(Errno::ISDIR.into());
            }
            let mut entries = base_entries();
            for (name, kind) in ctx.shared().resolver().virtual_entries(&guest) {
                entries.push(DirEnt {
                    ino: hashed_ino(&name),
                    kind: dirent_kind(Some(kind)),
                    name,
                });
            }
            (DescKind::Dir { entries, pos: 0 }, guest)
        }
        Node::VirtualLink { .. } => return Err(Errno::LOOP.into()),
        Node::Mounted { mount, rel, guest } => {
            let read_only = ctx.shared().is_read_only(mount);
            if wants_write && read_only {
                return Err(Errno::ROFS.into());
            }
            let sh = ctx.shared();
            let vfs = sh.vfs(mount).ok_or(Errno::NOENT)?.clone();
            let stat = match vfs.stat(&rel, true) {
                Ok(stat) => Some(stat),
                Err(Errno::NOENT) => None,
                Err(e) => return Err(vfs_fault(sh, e)),
            };
            let is_dir = stat
                .map(|st| st.file_type == FileType::Dir)
                .unwrap_or(false);
            if flags & O_DIRECTORY != 0 && stat.is_some() && !is_dir {
                return Err(Errno::NOTDIR.into());
            }
            if is_dir {
                if wants_write {
                    return Err(Errno::ISDIR.into());
                }
                let listing = vfs.readdir(&rel).map_err(|e| vfs_fault(sh, e))?;
                let mut entries = base_entries();
                for item in listing {
                    let ino = if item.ino == 0 {
                        hashed_ino(&format!("{guest}/{}", item.name))
                    } else {
                        item.ino
                    };
                    entries.push(DirEnt {
                        ino,
                        kind: dirent_kind(item.file_type),
                        name: item.name,
                    });
                }
                // Mount points that live under this directory are part of the listing too.
                for (name, kind) in ctx.shared().resolver().virtual_entries(&guest) {
                    if !entries.iter().any(|e| e.name == name) {
                        entries.push(DirEnt {
                            ino: hashed_ino(&name),
                            kind: dirent_kind(Some(kind)),
                            name,
                        });
                    }
                }
                (DescKind::Dir { entries, pos: 0 }, guest)
            } else {
                let options = OpenOptions {
                    read: access == O_RDONLY || access == O_RDWR,
                    write: access == O_WRONLY || access == O_RDWR,
                    append: flags & O_APPEND != 0,
                    create: flags & O_CREAT != 0,
                    create_new: flags & O_EXCL != 0 && flags & O_CREAT != 0,
                    truncate: flags & O_TRUNC != 0 && access != O_RDONLY,
                    mode: mode & !ctx.task.umask & 0o7777,
                };
                let sh = ctx.shared();
                let file = vfs.open(&rel, &options).map_err(|e| vfs_fault(sh, e))?;
                (
                    DescKind::File {
                        file,
                        mount,
                        closed: false,
                    },
                    guest,
                )
            }
        }
    };

    let sh = ctx.shared();
    let limit = sh.limits.open_files;
    let key = sh
        .descs
        .insert(Desc::new(kind, guest, status), limit.saturating_mul(4))
        .ok_or(Errno::MFILE)?;
    match ctx.task.fds.alloc(Fd { desc: key, cloexec }, 0, limit) {
        Ok(fd) => Ok(fd),
        Err(e) => {
            ctx.shared().release_desc(key);
            Err(e.into())
        }
    }
}

fn base_entries() -> Vec<DirEnt> {
    vec![
        DirEnt {
            ino: 1,
            kind: DT_DIR,
            name: ".".to_string(),
        },
        DirEnt {
            ino: 1,
            kind: DT_DIR,
            name: "..".to_string(),
        },
    ]
}

fn sys_statx(
    ctx: &mut Ctx,
    s: &mut dyn Suspend,
    dirfd: i32,
    path_at: u32,
    flags: i32,
    buffer_at: u32,
) -> Sys {
    let path = memory(s).cstr(path_at)?;
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let (stat, name, rdev) = if flags & AT_EMPTY_PATH != 0 && path.is_empty() {
        let entry = ctx.task.fds.get(dirfd)?;
        let sh = ctx.shared();
        let desc = sh.descs.get(entry.desc).ok_or(Errno::BADF)?;
        let path = desc.path.clone();
        match &desc.kind {
            DescKind::File { .. } => {
                let Some(Desc {
                    kind: DescKind::File { file, .. },
                    ..
                }) = sh.descs.get_mut(entry.desc)
                else {
                    return Err(Errno::BADF.into());
                };
                let stat = file.stat().map_err(|e| vfs_fault(sh, e))?;
                (stat, path, (0, 0))
            }
            DescKind::Dir { .. } => (Stat::dir(), path, (0, 0)),
            DescKind::PipeRead(_) | DescKind::PipeWrite(_) => (
                Stat {
                    mode: 0o600,
                    ..Stat::file(0)
                },
                "pipe:".to_string(),
                (0, 0),
            ),
            DescKind::Stdin | DescKind::Stdout(_) => (
                Stat {
                    mode: 0o620,
                    ..Stat::file(0)
                },
                path,
                (5, 0),
            ),
            DescKind::Memory { data, .. } => (Stat::file(data.len() as u64), path, (0, 0)),
            _ => (
                Stat {
                    mode: 0o666,
                    ..Stat::file(0)
                },
                path,
                (1, 3),
            ),
        }
    } else {
        match resolve(ctx, dirfd, &path, follow)? {
            Node::Program { guest, .. } => (
                Stat {
                    mode: 0o755,
                    ..Stat::file(0)
                },
                guest,
                (0, 0),
            ),
            Node::Device(device) => {
                let (mode, rdev) = match device {
                    Device::Null => (0o666, (1, 3)),
                    Device::Zero => (0o666, (1, 5)),
                    Device::Random => (0o666, (1, 9)),
                    Device::Tty | Device::Fd(_) => (0o620, (5, 0)),
                };
                (
                    Stat {
                        mode,
                        ..Stat::file(0)
                    },
                    path.clone(),
                    rdev,
                )
            }
            Node::Synth { guest } => {
                let content = proc_content(ctx, &guest);
                (
                    Stat {
                        mode: 0o444,
                        ..Stat::file(content.len() as u64)
                    },
                    guest,
                    (0, 0),
                )
            }
            Node::VirtualDir { guest } => (Stat::dir(), guest, (0, 0)),
            Node::VirtualLink { target } => {
                (Stat::symlink(target.len() as u64), path.clone(), (0, 0))
            }
            Node::Mounted { mount, rel, guest } => {
                let sh = ctx.shared();
                let vfs = sh.vfs(mount).ok_or(Errno::NOENT)?.clone();
                let stat = vfs.stat(&rel, follow).map_err(|e| vfs_fault(sh, e))?;
                (stat, guest, (0, 0))
            }
        }
    };
    // A character device has to look like one, or `test -c` and `ls` disagree.
    let mut stat = stat;
    if rdev != (0, 0) {
        stat.file_type = FileType::File;
    }
    let mut m = memory(s);
    let mut adjusted = stat;
    if rdev != (0, 0) {
        adjusted.mode = stat.mode;
    }
    write_statx(&mut m, buffer_at, &adjusted, &name, rdev)?;
    if rdev != (0, 0) {
        // Patch the type bits to S_IFCHR, which `write_statx` cannot express through `Stat`.
        let mode_at = buffer_at.wrapping_add(28);
        let mode = (S_IFCHR
            | if adjusted.mode == 0 {
                0o666
            } else {
                adjusted.mode
            }) as u16;
        m.put_u16(mode_at, mode)?;
    }
    Ok(0)
}

fn sys_read(ctx: &mut Ctx, s: &mut dyn Suspend, fd: i32, at: u32, length: u32) -> Sys {
    if length == 0 {
        return Ok(0);
    }
    // Staged first: the Vfs may block, and guest memory must not be touched until the call
    // has actually committed.
    let mut staging = vec![0u8; length.min(1 << 20) as usize];
    let produced = read_bytes(ctx, fd, &mut staging)?;
    memory(s).put(at, staging.get(..produced).unwrap_or(&[]))?;
    Ok(produced as i32)
}

/// Read from a descriptor into `staging`, with no guest involved.
///
/// This is the whole of `read(2)` except the copy into guest memory, which is why a host
/// command gets pipes, redirection and `/dev/*` for free rather than a parallel
/// implementation: it reads through the same descriptions the guests do.
pub(crate) fn read_bytes(ctx: &mut Ctx, fd: i32, staging: &mut [u8]) -> Result<usize, Fault> {
    if staging.is_empty() {
        return Ok(0);
    }
    let entry = ctx.task.fds.get(fd)?;
    let sh = ctx.shared();
    let desc = sh.descs.get(entry.desc).ok_or(Errno::BADF)?;
    if !desc.readable() {
        return Err(Errno::BADF.into());
    }
    let nonblocking = desc.is_nonblocking();
    let key = entry.desc;
    let produced = {
        let desc = sh.descs.get_mut(key).ok_or(Errno::BADF)?;
        match &mut desc.kind {
            DescKind::File { file, .. } => match file.read(staging) {
                Ok(n) => n,
                Err(e) => return Err(vfs_fault(sh, e)),
            },
            DescKind::Memory { data, pos } => {
                let start = (*pos).min(data.len());
                let available = data.get(start..).unwrap_or(&[]);
                let n = available.len().min(staging.len());
                if let (Some(dst), Some(src)) = (staging.get_mut(..n), available.get(..n)) {
                    dst.copy_from_slice(src);
                }
                *pos = start.saturating_add(n);
                n
            }
            DescKind::Null => 0,
            DescKind::Zero => staging.len(),
            DescKind::Random => {
                fill_random(staging);
                staging.len()
            }
            DescKind::Dir { .. } => return Err(Errno::ISDIR.into()),
            DescKind::PipeWrite(_) | DescKind::Stdout(_) => return Err(Errno::BADF.into()),
            DescKind::Stdin => {
                let stdin = &mut sh.stdin;
                if stdin.data.is_empty() {
                    if stdin.closed {
                        0
                    } else if nonblocking {
                        return Err(Errno::AGAIN.into());
                    } else {
                        stdin.wanted = true;
                        return Err(Fault::Block);
                    }
                } else {
                    // One line at a time, as a terminal in canonical mode would.
                    let mut count = 0;
                    while count < staging.len() {
                        match stdin.data.pop_front() {
                            Some(byte) => {
                                if let Some(slot) = staging.get_mut(count) {
                                    *slot = byte;
                                }
                                count = count.saturating_add(1);
                                if byte == b'\n' {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    stdin.wanted = false;
                    count
                }
            }
            DescKind::PipeRead(pipe_key) => {
                let pipe_key = *pipe_key;
                let pipe = sh.pipes.get_mut(pipe_key).ok_or(Errno::BADF)?;
                if pipe.data.is_empty() {
                    if pipe.writers == 0 {
                        0
                    } else if nonblocking {
                        return Err(Errno::AGAIN.into());
                    } else {
                        return Err(Fault::Block);
                    }
                } else {
                    let count = pipe.data.len().min(staging.len());
                    for index in 0..count {
                        if let (Some(slot), Some(byte)) =
                            (staging.get_mut(index), pipe.data.pop_front())
                        {
                            *slot = byte;
                        }
                    }
                    count
                }
            }
        }
    };
    Ok(produced)
}

fn sys_write(ctx: &mut Ctx, s: &mut dyn Suspend, fd: i32, at: u32, length: u32) -> Sys {
    let data = memory(s).bytes(at, length)?.to_vec();
    Ok(write_bytes(ctx, fd, &data)? as i32)
}

/// Write `data` to a descriptor, with no guest involved. The counterpart of [`read_bytes`].
pub(crate) fn write_bytes(ctx: &mut Ctx, fd: i32, data: &[u8]) -> Result<usize, Fault> {
    let entry = ctx.task.fds.get(fd)?;
    let sh = ctx.shared();
    let desc = sh.descs.get(entry.desc).ok_or(Errno::BADF)?;
    if !desc.writable() {
        return Err(Errno::BADF.into());
    }
    let nonblocking = desc.is_nonblocking();
    let key = entry.desc;
    // Whether the mount refuses writes is decided before the description is borrowed.
    let file_mount = match &desc.kind {
        DescKind::File { mount, .. } => Some(*mount),
        _ => None,
    };
    if let Some(mount) = file_mount {
        if sh.is_read_only(mount) {
            return Err(Errno::ROFS.into());
        }
    }
    let desc = sh.descs.get_mut(key).ok_or(Errno::BADF)?;
    match &mut desc.kind {
        DescKind::File { file, .. } => match file.write(data) {
            Ok(n) => Ok(n),
            Err(e) => Err(vfs_fault(sh, e)),
        },
        DescKind::Null | DescKind::Zero | DescKind::Random | DescKind::Memory { .. } => {
            Ok(data.len())
        }
        DescKind::Dir { .. } | DescKind::PipeRead(_) | DescKind::Stdin => Err(Errno::BADF.into()),
        DescKind::Stdout(which) => {
            let which = *which;
            Ok(sh.write_output(which, data))
        }
        DescKind::PipeWrite(pipe_key) => {
            let pipe_key = *pipe_key;
            let pipe = sh.pipes.get_mut(pipe_key).ok_or(Errno::BADF)?;
            if pipe.readers == 0 {
                let pid = ctx.task.pid;
                if let Some(record) = ctx.shared().record_mut(pid) {
                    record.raise(SIGPIPE);
                }
                return Err(Errno::PIPE.into());
            }
            let room = pipe.room();
            if room == 0 {
                return if nonblocking {
                    Err(Errno::AGAIN.into())
                } else {
                    Err(Fault::Block)
                };
            }
            let take = room.min(data.len());
            pipe.data.extend(data.get(..take).unwrap_or(&[]));
            Ok(take)
        }
    }
}

fn sys_ioctl(ctx: &mut Ctx, s: &mut dyn Suspend, fd: i32, request: u32, argument: u32) -> Sys {
    let entry = ctx.task.fds.get(fd)?;
    match request {
        FIOCLEX | FIONCLEX => {
            ctx.task.fds.set_cloexec(fd, request == FIOCLEX)?;
            return Ok(0);
        }
        FIONBIO => {
            let value = memory(s).u32(argument)?;
            let desc = ctx.shared().descs.get_mut(entry.desc).ok_or(Errno::BADF)?;
            desc.flags = if value != 0 {
                desc.flags | O_NONBLOCK
            } else {
                desc.flags & !O_NONBLOCK
            };
            return Ok(0);
        }
        FIONREAD => {
            let sh = ctx.shared();
            let desc = sh.descs.get(entry.desc).ok_or(Errno::BADF)?;
            let available = match &desc.kind {
                DescKind::PipeRead(key) => sh.pipes.get(*key).map(|p| p.data.len()).unwrap_or(0),
                DescKind::Stdin => sh.stdin.data.len(),
                DescKind::Memory { data, pos } => data.len().saturating_sub(*pos),
                _ => 0,
            };
            memory(s).put_u32(argument, available as u32)?;
            return Ok(0);
        }
        _ => {}
    }
    let sh = ctx.shared();
    let desc = sh.descs.get(entry.desc).ok_or(Errno::BADF)?;
    if !desc.is_tty() {
        return Err(Errno::NOTTY.into());
    }
    match request {
        // A window size makes isatty succeed and gives the shell a width. Terminal attributes
        // are refused, which puts BusyBox into its line-at-a-time path: right, because there
        // is no terminal here, only a pipe the host owns.
        TIOCGWINSZ => {
            let mut m = memory(s);
            m.put_u16(argument, 24)?;
            m.put_u16(argument.wrapping_add(2), 80)?;
            m.put_u16(argument.wrapping_add(4), 0)?;
            m.put_u16(argument.wrapping_add(6), 0)?;
            Ok(0)
        }
        TIOCGPGRP => {
            let pid = ctx.task.pid;
            let group = ctx.shared().record(pid).map(|r| r.pgid).unwrap_or(pid);
            memory(s).put_u32(argument, group as u32)?;
            Ok(0)
        }
        TIOCSPGRP | TIOCSWINSZ | TIOCSCTTY | TIOCNOTTY => Ok(0),
        _ => Err(Errno::NOTTY.into()),
    }
}

/// What one descriptor is ready for.
fn poll_one(sh: &Shared, desc_key: crate::slab::Key, events: i16) -> i16 {
    let Some(desc) = sh.descs.get(desc_key) else {
        return POLLNVAL;
    };
    let mut ready = 0i16;
    match &desc.kind {
        DescKind::PipeRead(key) => match sh.pipes.get(*key) {
            Some(pipe) if !pipe.data.is_empty() => ready |= POLLIN,
            Some(pipe) if pipe.writers == 0 => ready |= POLLHUP,
            _ => {}
        },
        DescKind::PipeWrite(key) => match sh.pipes.get(*key) {
            Some(pipe) if pipe.readers == 0 => ready |= POLLERR,
            Some(pipe) if pipe.room() > 0 => ready |= POLLOUT,
            _ => {}
        },
        DescKind::Stdin => {
            if !sh.stdin.data.is_empty() || sh.stdin.closed {
                ready |= POLLIN;
            }
        }
        DescKind::Stdout(_) => ready |= POLLOUT,
        _ => ready |= POLLIN | POLLOUT,
    }
    ready & (events | POLLHUP | POLLERR | POLLNVAL)
}

fn sys_ppoll(ctx: &mut Ctx, s: &mut dyn Suspend, fds_at: u32, count: u32, timeout_at: u32) -> Sys {
    let until = if timeout_at == 0 {
        None
    } else {
        let requested = memory(s).timespec(timeout_at)?;
        Some(deadline(ctx, requested))
    };
    let mut results: Vec<(u32, i16)> = Vec::new();
    let mut ready: i32 = 0;
    for index in 0..count.min(1024) {
        let entry_at = fds_at.wrapping_add(index.wrapping_mul(8));
        let m = memory(s);
        let fd = m.i32_at(entry_at)?;
        let events = m.u32(entry_at.wrapping_add(4))? as i16;
        let revents = if fd < 0 {
            0
        } else {
            match ctx.task.fds.get(fd) {
                Ok(entry) => poll_one(ctx.shared(), entry.desc, events),
                Err(_) => POLLNVAL,
            }
        };
        if revents != 0 {
            ready = ready.saturating_add(1);
        }
        results.push((entry_at.wrapping_add(6), revents));
    }
    if ready == 0 {
        match until {
            Some(when) if Instant::now() < when => {
                let pid = ctx.task.pid;
                if let Some(record) = ctx.shared().record_mut(pid) {
                    record.wake_at = Some(when);
                }
                return Err(Fault::Block);
            }
            None => {
                if ctx.task.fds.get(0).is_ok() {
                    ctx.shared().stdin.wanted = true;
                }
                return Err(Fault::Block);
            }
            Some(_) => {}
        }
    }
    let mut m = memory(s);
    for (at, revents) in results {
        m.put_u16(at, revents as u16)?;
    }
    Ok(ready)
}

fn sys_pselect(
    ctx: &mut Ctx,
    s: &mut dyn Suspend,
    count: i32,
    read_at: u32,
    write_at: u32,
    except_at: u32,
    timeout_at: u32,
) -> Sys {
    let count = count.clamp(0, 1024) as u32;
    let until = if timeout_at == 0 {
        None
    } else {
        let requested = memory(s).timespec(timeout_at)?;
        Some(deadline(ctx, requested))
    };
    let read_set = read_fd_set(s, read_at, count)?;
    let write_set = read_fd_set(s, write_at, count)?;
    let except_set = read_fd_set(s, except_at, count)?;
    let mut readable = vec![false; count as usize];
    let mut writable = vec![false; count as usize];
    let mut ready: i32 = 0;
    for fd in 0..count as usize {
        let wants_read = read_set.get(fd).copied().unwrap_or(false);
        let wants_write = write_set.get(fd).copied().unwrap_or(false);
        if !wants_read && !wants_write && !except_set.get(fd).copied().unwrap_or(false) {
            continue;
        }
        let events =
            (if wants_read { POLLIN } else { 0 }) | (if wants_write { POLLOUT } else { 0 });
        let revents = match ctx.task.fds.get(fd as i32) {
            Ok(entry) => poll_one(ctx.shared(), entry.desc, events),
            Err(_) => POLLNVAL,
        };
        if wants_read && revents & (POLLIN | POLLHUP | POLLERR) != 0 {
            if let Some(slot) = readable.get_mut(fd) {
                *slot = true;
            }
            ready = ready.saturating_add(1);
        }
        if wants_write && revents & (POLLOUT | POLLERR) != 0 {
            if let Some(slot) = writable.get_mut(fd) {
                *slot = true;
            }
            ready = ready.saturating_add(1);
        }
    }
    if ready == 0 {
        match until {
            Some(when) if Instant::now() < when => {
                let pid = ctx.task.pid;
                if let Some(record) = ctx.shared().record_mut(pid) {
                    record.wake_at = Some(when);
                }
                return Err(Fault::Block);
            }
            None => return Err(Fault::Block),
            Some(_) => {}
        }
    }
    write_fd_set(s, read_at, &readable)?;
    write_fd_set(s, write_at, &writable)?;
    write_fd_set(s, except_at, &vec![false; count as usize])?;
    Ok(ready)
}

fn read_fd_set(s: &mut dyn Suspend, at: u32, count: u32) -> Result<Vec<bool>, Errno> {
    let mut out = vec![false; count as usize];
    if at == 0 {
        return Ok(out);
    }
    let m = memory(s);
    for fd in 0..count {
        #[allow(
            clippy::integer_division,
            reason = "fd bitmap: word index and bit index"
        )]
        let word = m.u32(at.wrapping_add((fd / 32).wrapping_mul(4)))?;
        if let Some(slot) = out.get_mut(fd as usize) {
            *slot = word & (1 << (fd % 32)) != 0;
        }
    }
    Ok(out)
}

fn write_fd_set(s: &mut dyn Suspend, at: u32, values: &[bool]) -> Result<(), Errno> {
    if at == 0 {
        return Ok(());
    }
    let words = (values.len() as u32).div_ceil(32);
    let mut m = memory(s);
    for word_index in 0..words {
        let mut bits = 0u32;
        for offset in 0..32u32 {
            let fd = word_index.saturating_mul(32).saturating_add(offset) as usize;
            if values.get(fd).copied().unwrap_or(false) {
                bits |= 1 << offset;
            }
        }
        m.put_u32(at.wrapping_add(word_index.wrapping_mul(4)), bits)?;
    }
    Ok(())
}
