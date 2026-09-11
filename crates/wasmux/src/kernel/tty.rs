//! The terminal a session may be given, and the `termios` the guest sees.
//!
//! A sandbox has no terminal unless the embedder says otherwise, and most embedders never
//! should: an agent's standard streams are buffers, and a program told it has a terminal
//! starts behaving like it — `jq` emits colour, a shell reaches for its line editor. So this
//! whole module hangs off [`Command::terminal`](crate::Command::terminal), which is off by
//! default and compiled out entirely without the `tty` feature.
//!
//! When there *is* a terminal, the state here is the authority and the embedder follows it.
//! The guest asks for raw mode by clearing `ICANON` with `TCSETS`; the host reads that back
//! through [`Session::terminal_raw`](crate::Session::terminal_raw) and puts its own terminal
//! into raw mode to match. Both ends therefore agree on who echoes, which is the only way to
//! avoid every keystroke appearing twice.

use crate::abi::*;

/// `struct termios` as the Linux `asm-generic` ABI lays it out: four flag words, the line
/// discipline, and the control characters. 36 bytes.
///
/// musl's own `struct termios` is larger — it carries `NCCS` of 32 and two speed fields — but
/// it passes the whole thing to `ioctl` and reads back only what the kernel filled, exactly
/// as it does on a real kernel. Writing the kernel's 36 bytes is therefore right, and writing
/// musl's 60 would be wrong.
pub(crate) const TERMIOS_BYTES: usize = 17 + NCCS_KERNEL;

/// The terminal settings a guest can read and change.
#[derive(Clone)]
pub(crate) struct Termios {
    pub(crate) iflag: u32,
    pub(crate) oflag: u32,
    pub(crate) cflag: u32,
    pub(crate) lflag: u32,
    pub(crate) line: u8,
    pub(crate) cc: [u8; NCCS_KERNEL],
}

impl Termios {
    /// What a terminal looks like before anything has touched it: canonical mode, echo on,
    /// signals on. The same settings a login shell inherits on Linux.
    pub(crate) fn cooked() -> Termios {
        let mut cc = [0u8; NCCS_KERNEL];
        cc[VINTR] = 0x03; // ^C
        cc[VQUIT] = 0x1c; // ^\
        cc[VERASE] = 0x7f; // del
        cc[VKILL] = 0x15; // ^U
        cc[VEOF] = 0x04; // ^D
        cc[VTIME] = 0;
        cc[VMIN] = 1;
        cc[VSUSP] = 0x1a; // ^Z
        cc[VEOL] = 0;
        cc[VWERASE] = 0x17; // ^W
        Termios {
            iflag: ICRNL | IXON,
            oflag: OPOST | ONLCR,
            cflag: B38400 | CS8 | CREAD,
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN,
            line: 0,
            cc,
        }
    }

    /// Whether the guest has taken over line editing.
    ///
    /// `ICANON` is the one that decides it: with canonical mode off the guest wants a
    /// keystroke at a time and does its own echoing and erasing. The host has to match, or
    /// both ends echo.
    pub(crate) fn raw(&self) -> bool {
        self.lflag & ICANON == 0
    }

    /// Whether the guest wants Ctrl-C and friends to raise signals rather than arrive as
    /// bytes. hush clears this for the duration of a line and restores it to run a command.
    pub(crate) fn signals(&self) -> bool {
        self.lflag & ISIG != 0
    }

    pub(crate) fn encode(&self) -> [u8; TERMIOS_BYTES] {
        let mut out = [0u8; TERMIOS_BYTES];
        out[0..4].copy_from_slice(&self.iflag.to_le_bytes());
        out[4..8].copy_from_slice(&self.oflag.to_le_bytes());
        out[8..12].copy_from_slice(&self.cflag.to_le_bytes());
        out[12..16].copy_from_slice(&self.lflag.to_le_bytes());
        out[16] = self.line;
        out[17..TERMIOS_BYTES].copy_from_slice(&self.cc);
        out
    }

    /// Read what the guest wrote. The slice is guest-supplied, so every field is taken by a
    /// checked accessor and a short one is refused rather than trusted.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Termios> {
        let word = |at: usize| -> Option<u32> {
            let slice = bytes.get(at..at.checked_add(4)?)?;
            let mut buf = [0u8; 4];
            buf.copy_from_slice(slice);
            Some(u32::from_le_bytes(buf))
        };
        let mut cc = [0u8; NCCS_KERNEL];
        cc.copy_from_slice(bytes.get(17..TERMIOS_BYTES)?);
        Some(Termios {
            iflag: word(0)?,
            oflag: word(4)?,
            cflag: word(8)?,
            lflag: word(12)?,
            line: *bytes.get(16)?,
            cc,
        })
    }
}

/// The terminal a session was given: its size, and how it is currently set.
#[derive(Clone)]
pub(crate) struct Terminal {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) termios: Termios,
}

impl Terminal {
    pub(crate) fn new(cols: u16, rows: u16) -> Terminal {
        Terminal {
            cols,
            rows,
            termios: Termios::cooked(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_terminal_is_canonical_and_echoing() {
        let t = Termios::cooked();
        assert!(!t.raw());
        assert!(t.signals());
    }

    #[test]
    fn clearing_icanon_is_what_raw_means() {
        let mut t = Termios::cooked();
        t.lflag &= !ICANON;
        assert!(t.raw());
    }

    #[test]
    fn a_round_trip_through_the_guest_layout_keeps_every_field() {
        let mut t = Termios::cooked();
        t.lflag &= !(ICANON | ECHO | ISIG);
        t.cc[VMIN] = 7;
        let back = Termios::decode(&t.encode()).unwrap();
        assert_eq!(back.iflag, t.iflag);
        assert_eq!(back.oflag, t.oflag);
        assert_eq!(back.cflag, t.cflag);
        assert_eq!(back.lflag, t.lflag);
        assert_eq!(back.cc[VMIN], 7);
        assert!(back.raw());
        assert!(!back.signals());
    }

    #[test]
    fn a_short_buffer_is_refused_rather_than_read_past() {
        assert!(Termios::decode(&[0u8; TERMIOS_BYTES - 1]).is_none());
        assert!(Termios::decode(&[]).is_none());
    }

    #[test]
    fn the_layout_is_the_one_the_kernel_abi_specifies() {
        assert_eq!(TERMIOS_BYTES, 36);
        assert_eq!(Termios::cooked().encode().len(), 36);
    }
}
