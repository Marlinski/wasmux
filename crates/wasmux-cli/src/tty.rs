//! This process's own terminal, kept in step with the guest's.
//!
//! The sandbox holds the authoritative `termios`: the guest sets it with `TCSETS` and the
//! session reports it back through `Session::terminal_raw` and `Session::terminal_signals`.
//! This module is the other half — it puts the real terminal into the same state, because
//! two ends that disagree about who echoes produce either two copies of every keystroke or
//! none at all.
//!
//! Only what the guest actually changes is changed here. BusyBox's line editor clears
//! `ICANON`, `ECHO` and `ECHONL`, adds `ISIG` when it wants Ctrl-C as a byte, and sets
//! `VMIN`/`VTIME` for a blocking one-character read. It leaves `OPOST` alone, so a newline
//! still becomes a carriage return and line feed on the way out; `cfmakeraw` would clear it
//! and stairstep every line of output.

/// The terminal's size, if there is a terminal.
#[cfg(all(unix, feature = "tty"))]
pub(crate) fn size() -> Option<(u16, u16)> {
    // SAFETY: `ioctl` fills a `winsize` we own, and the result is checked before it is read.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if rc != 0 || ws.ws_col == 0 {
        return None;
    }
    Some((ws.ws_col, ws.ws_row))
}

#[cfg(not(all(unix, feature = "tty")))]
pub(crate) fn size() -> Option<(u16, u16)> {
    None
}

/// The terminal's settings as they were before this process touched them, restored when this
/// is dropped — including on the error paths out of `main`, which is the reason it is a guard
/// and not a pair of functions.
#[cfg(all(unix, feature = "tty"))]
pub(crate) struct HostTerminal {
    original: libc::termios,
    /// What the guest last asked for, so an unchanged state costs no syscall.
    applied: Option<(bool, bool)>,
}

#[cfg(all(unix, feature = "tty"))]
impl HostTerminal {
    /// Take the current settings, to mirror the guest onto and to restore at the end.
    pub(crate) fn take() -> Option<HostTerminal> {
        // SAFETY: `tcgetattr` fills a `termios` we own; the result is checked.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return None;
        }
        Some(HostTerminal {
            original,
            applied: None,
        })
    }

    /// Match the guest: `raw` means it has taken over echoing and erasing, `signals` means it
    /// still wants Ctrl-C to raise one rather than arrive as a byte.
    pub(crate) fn mirror(&mut self, raw: bool, signals: bool) {
        if self.applied == Some((raw, signals)) {
            return;
        }
        self.applied = Some((raw, signals));
        let mut next = self.original;
        if raw {
            next.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHONL);
            next.c_cc[libc::VMIN] = 1;
            next.c_cc[libc::VTIME] = 0;
        }
        if !signals {
            next.c_lflag &= !libc::ISIG;
        }
        // A failure here is not worth ending the session over: the shell still runs, the
        // keystrokes are just echoed by the wrong end.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &next) };
    }
}

#[cfg(all(unix, feature = "tty"))]
impl Drop for HostTerminal {
    fn drop(&mut self) {
        if self.applied.is_some() {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original) };
        }
    }
}

/// Put the terminal into whatever state the guest has asked for.
///
/// The only entry point the driving loop needs, so the loop itself carries no `cfg`: without
/// a terminal, or without the feature, `host` is `None` and this does nothing.
#[cfg(all(unix, feature = "tty"))]
pub(crate) fn follow(host: Option<&mut HostTerminal>, session: &wasmux::Session) {
    if let Some(host) = host {
        host.mirror(session.terminal_raw(), session.terminal_signals());
    }
}

/// Without a terminal, or without the feature, there is nothing to keep in step.
#[cfg(not(all(unix, feature = "tty")))]
pub(crate) struct HostTerminal;

#[cfg(not(all(unix, feature = "tty")))]
impl HostTerminal {
    pub(crate) fn take() -> Option<HostTerminal> {
        None
    }
}

#[cfg(not(all(unix, feature = "tty")))]
pub(crate) fn follow(_host: Option<&mut HostTerminal>, _session: &wasmux::Session) {}
