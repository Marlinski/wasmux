//! Per-process state, and the process table other processes can see.
//!
//! The split matters. [`Task`] is private to one process and is only ever touched by that
//! process's own syscalls, so it needs no synchronization. [`ProcRecord`] is what `kill`,
//! `wait4` and `ps` reach across process boundaries, so it lives in the kernel's shared table.
//! Signals are delivered by the guest itself: the kernel raises a flag in the process's memory
//! and musl calls back after the next syscall, which is why there is no asynchronous delivery
//! and a compute loop is interrupted by the tick fuse rather than by a signal.

use crate::abi::*;
use crate::kernel::fd::FdTable;
use std::time::Instant;

/// Signal numbers run 1 to 63; slot 0 is unused so that `sig` indexes directly.
pub(crate) const NSIG: usize = 64;

/// What the guest installed with `rt_sigaction`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct SigAction {
    /// Guest function pointer, or [`SIG_DFL`] / [`SIG_IGN`].
    pub(crate) handler: u32,
    /// `SA_*` flags.
    pub(crate) flags: u32,
    /// Signals blocked while the handler runs. Recorded but not yet applied; see `docs/DESIGN.md`.
    pub(crate) mask: u64,
}

impl SigAction {
    fn is_ignored(&self, sig: i32) -> bool {
        self.handler == SIG_IGN || (self.handler == SIG_DFL && default_ignored(sig))
    }
}

/// One process's private state.
pub(crate) struct Task {
    pub(crate) pid: i32,
    pub(crate) fds: FdTable,
    /// Absolute, normalized guest path.
    pub(crate) cwd: String,
    pub(crate) umask: u32,
    pub(crate) sigact: [SigAction; NSIG],
    pub(crate) sigmask: u64,
    /// The path this process was executed from, which `/proc/self/exe` reports.
    pub(crate) exe: String,
    /// `argv[0]`, which is how BusyBox picks an applet, and what `ps` shows.
    pub(crate) comm: String,
}

impl Task {
    pub(crate) fn new(pid: i32, fds: FdTable) -> Task {
        Task {
            pid,
            fds,
            cwd: "/".to_string(),
            umask: 0o022,
            sigact: [SigAction::default(); NSIG],
            sigmask: 0,
            exe: String::new(),
            comm: String::new(),
        }
    }

    pub(crate) fn action(&self, sig: i32) -> SigAction {
        if sig <= 0 {
            return SigAction::default();
        }
        self.sigact.get(sig as usize).copied().unwrap_or_default()
    }

    pub(crate) fn set_action(&mut self, sig: i32, action: SigAction) {
        if sig > 0 {
            if let Some(slot) = self.sigact.get_mut(sig as usize) {
                *slot = action;
            }
        }
    }

    /// `execve` keeps the descriptor table and the mask, but every handler that was pointing
    /// into the old program's code is reset. Ignored signals stay ignored, as Linux does.
    pub(crate) fn reset_handlers_for_exec(&mut self) {
        for action in self.sigact.iter_mut() {
            if action.handler != SIG_IGN {
                *action = SigAction::default();
            }
        }
    }
}

/// The part of a process that other processes can see.
pub(crate) struct ProcRecord {
    pub(crate) pid: i32,
    pub(crate) ppid: i32,
    pub(crate) pgid: i32,
    pub(crate) sid: i32,
    /// Bit per signal, raised by `kill` and lowered when the guest takes delivery.
    pub(crate) pending: u64,
    /// Set once the process is gone; the record survives until a `wait4` collects it.
    pub(crate) exit: Option<i32>,
    /// When a sleeping process should be woken.
    pub(crate) wake_at: Option<Instant>,
    /// `argv[0]`, for `ps` and `/proc/<pid>/comm`.
    #[allow(dead_code)]
    pub(crate) comm: String,
}

impl ProcRecord {
    pub(crate) fn new(pid: i32, ppid: i32, pgid: i32, sid: i32, comm: String) -> ProcRecord {
        ProcRecord {
            pid,
            ppid,
            pgid,
            sid,
            pending: 0,
            exit: None,
            wake_at: None,
            comm,
        }
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.exit.is_none()
    }

    pub(crate) fn raise(&mut self, sig: i32) {
        if sig > 0 && (sig as usize) < NSIG {
            self.pending |= bit(sig);
        }
    }
}

/// The bit for one signal.
pub(crate) fn bit(sig: i32) -> u64 {
    if sig > 0 && (sig as usize) < NSIG {
        1u64 << (sig as u32)
    } else {
        0
    }
}

/// Signals whose default action is to do nothing. Stop and continue are here too: job control
/// stop is not implemented, and silently ignoring is closer to right than killing.
pub(crate) fn default_ignored(sig: i32) -> bool {
    matches!(
        sig,
        SIGCHLD | SIGURG | SIGWINCH | SIGCONT | SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU
    )
}

/// Whether the default action for `sig` is to terminate the process.
pub(crate) fn default_kills(sig: i32) -> bool {
    !default_ignored(sig)
}

/// Which pending signals would actually do something now.
///
/// Returns the deliverable set, and the set to discard because it is ignored. `SIGKILL` and
/// `SIGSTOP` cannot be blocked, exactly as on Linux.
pub(crate) fn triage(pending: u64, mask: u64, task: &Task) -> (u64, u64) {
    if pending == 0 {
        return (0, 0);
    }
    let mut live = 0u64;
    let mut discard = 0u64;
    for sig in 1..NSIG as i32 {
        let b = bit(sig);
        if pending & b == 0 {
            continue;
        }
        if task.action(sig).is_ignored(sig) {
            discard |= b;
        } else if mask & b == 0 || sig == SIGKILL || sig == SIGSTOP {
            live |= b;
        }
    }
    (live, discard)
}

/// The wait status for a normal exit.
pub(crate) fn status_exited(code: i32) -> i32 {
    (code & 0xff) << 8
}

/// The wait status for a death by signal.
pub(crate) fn status_signaled(sig: i32) -> i32 {
    sig & 0x7f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::fd::FdTable;

    fn task() -> Task {
        Task::new(1, FdTable::new())
    }

    #[test]
    fn ignored_signals_are_discarded_not_delivered() {
        let t = task();
        let (live, discard) = triage(bit(SIGCHLD) | bit(SIGTERM), 0, &t);
        assert_eq!(live, bit(SIGTERM), "SIGCHLD defaults to ignored");
        assert_eq!(discard, bit(SIGCHLD));
    }

    #[test]
    fn a_handler_makes_an_otherwise_ignored_signal_deliverable() {
        let mut t = task();
        t.set_action(
            SIGCHLD,
            SigAction {
                handler: 0x1234,
                flags: 0,
                mask: 0,
            },
        );
        let (live, discard) = triage(bit(SIGCHLD), 0, &t);
        assert_eq!(live, bit(SIGCHLD));
        assert_eq!(discard, 0);
    }

    #[test]
    fn explicit_ignore_beats_a_pending_signal() {
        let mut t = task();
        t.set_action(
            SIGINT,
            SigAction {
                handler: SIG_IGN,
                flags: 0,
                mask: 0,
            },
        );
        let (live, discard) = triage(bit(SIGINT), 0, &t);
        assert_eq!(live, 0);
        assert_eq!(discard, bit(SIGINT));
    }

    #[test]
    fn blocking_defers_everything_except_kill_and_stop() {
        let t = task();
        let all = bit(SIGTERM) | bit(SIGKILL) | bit(SIGSTOP);
        let (live, _) = triage(all, all, &t);
        assert_eq!(
            live,
            bit(SIGKILL),
            "SIGSTOP is ignored by default here, SIGKILL is not"
        );
    }

    #[test]
    fn out_of_range_signals_are_inert() {
        let mut t = task();
        assert_eq!(bit(0), 0);
        assert_eq!(bit(-1), 0);
        assert_eq!(bit(64), 0);
        assert_eq!(bit(9999), 0);
        t.set_action(
            9999,
            SigAction {
                handler: 1,
                flags: 0,
                mask: 0,
            },
        );
        assert_eq!(t.action(9999), SigAction::default());
    }

    #[test]
    fn wait_status_encoding_matches_the_shell() {
        assert_eq!(status_exited(0), 0);
        assert_eq!(status_exited(7) >> 8, 7);
        assert_eq!(status_signaled(SIGKILL) & 0x7f, 9);
        // What hush reports as $? for each.
        assert_eq!((status_exited(7) >> 8) & 0xff, 7);
        assert_eq!(128 + (status_signaled(SIGKILL) & 0x7f), 137);
    }

    #[test]
    fn exec_resets_handlers_but_keeps_ignores() {
        let mut t = task();
        t.set_action(
            SIGINT,
            SigAction {
                handler: 0xabc,
                flags: 0,
                mask: 0,
            },
        );
        t.set_action(
            SIGTERM,
            SigAction {
                handler: SIG_IGN,
                flags: 0,
                mask: 0,
            },
        );
        t.reset_handlers_for_exec();
        assert_eq!(t.action(SIGINT).handler, SIG_DFL);
        assert_eq!(t.action(SIGTERM).handler, SIG_IGN);
    }
}
