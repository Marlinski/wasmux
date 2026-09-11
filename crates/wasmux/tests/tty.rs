//! The terminal a session can be given, and what it means to leave it alone.
//!
//! The default matters more than the feature here. An agent's standard streams are buffers,
//! and a program that believes in a terminal writes a screen rather than text: colour codes,
//! cursor movement, a prompt in the captured output. So the first test is the one asserting
//! nothing changed, and it runs whether or not the feature is compiled in.
// A test asserts; `unwrap` and `panic!` are how it does that. The library denies them because
// a panic there aborts the consumer's whole component, which is not true of a test binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use wasmux::{MemVfs, Sandbox};

fn sandbox() -> Sandbox {
    Sandbox::builder()
        .mount("/", MemVfs::new())
        .mount("/tmp", MemVfs::new())
        .build()
        .expect("build")
}

#[test]
fn without_a_terminal_the_guest_is_told_there_is_none() {
    let out = sandbox()
        .shell("[ -t 0 ] && echo yes || echo no; [ -t 1 ] && echo yes || echo no")
        .output()
        .expect("run");
    assert_eq!(out.stdout_string().trim(), "no\nno");
}

#[test]
fn without_a_terminal_stty_refuses_the_way_it_would_on_a_pipe() {
    let out = sandbox()
        .shell("stty -a 2>&1 | head -1")
        .output()
        .expect("run");
    assert!(
        out.stdout_string().contains("Not a tty"),
        "expected ENOTTY, got {:?}",
        out.stdout_string()
    );
}

/// The shell's own output must not gain a prompt when nobody asked for a terminal, because
/// that output is what an agent reads.
#[test]
fn without_a_terminal_nothing_is_added_to_the_output() {
    let out = sandbox().shell("echo one; echo two").output().expect("run");
    assert_eq!(out.stdout_string(), "one\ntwo\n");
}

#[cfg(feature = "tty")]
mod with_a_terminal {
    use super::*;

    #[test]
    fn the_guest_is_told_its_streams_are_a_terminal() {
        let out = sandbox()
            .command("sh")
            .arg("-c")
            .arg("[ -t 0 ] && echo yes || echo no")
            .terminal(100, 40)
            .output()
            .expect("run");
        assert_eq!(out.stdout_string().trim(), "yes");
    }

    #[test]
    fn the_size_the_embedder_gave_is_the_size_the_guest_sees() {
        let out = sandbox()
            .command("sh")
            .arg("-c")
            .arg("stty size")
            .terminal(100, 40)
            .output()
            .expect("run");
        // `stty size` prints rows then columns, which is the order of the struct and the
        // reverse of the order they are passed in.
        assert_eq!(out.stdout_string().trim(), "40 100");
    }

    /// The host has to follow the guest, so what the guest set has to survive being read back.
    #[test]
    fn settings_written_by_the_guest_come_back_out() {
        let out = sandbox()
            .command("sh")
            .arg("-c")
            .arg("stty -echo; stty -a 2>&1 | grep -o -- '-echo' | head -1")
            .terminal(80, 24)
            .output()
            .expect("run");
        assert_eq!(out.stdout_string().trim(), "-echo");
    }

    /// A session that was never given a terminal must report false rather than panic, so a
    /// host can poll it unconditionally.
    #[test]
    fn a_session_without_a_terminal_is_never_raw() {
        let mut session = sandbox().shell("echo hi").spawn().expect("spawn");
        assert!(!session.terminal_raw());
        assert!(!session.terminal_signals());
        while let wasmux::Progress::Yielded | wasmux::Progress::Waiting(_) = session
            .step(wasmux::Budget::syscalls(10_000))
            .expect("step")
        {
            assert!(!session.terminal_raw());
        }
    }

    /// Canonical mode is the state a terminal starts in: the host is still doing the echoing
    /// until the guest says otherwise.
    #[test]
    fn a_fresh_terminal_is_not_raw() {
        let session = sandbox()
            .command("sh")
            .arg("-c")
            .arg("echo hi")
            .terminal(80, 24)
            .spawn()
            .expect("spawn");
        assert!(!session.terminal_raw());
        assert!(session.terminal_signals());
    }

    /// The whole point of `terminal_raw`: a guest that turns off canonical mode is asking the
    /// host to stop echoing, and the host can only find out by being told.
    #[test]
    fn turning_off_canonical_mode_is_visible_to_the_host() {
        let mut session = sandbox()
            .command("sh")
            .arg("-c")
            .arg("stty -icanon -echo; echo done")
            .terminal(80, 24)
            .spawn()
            .expect("spawn");
        // A step at a time, because a generous budget runs the whole script inside one and
        // the state to observe is the one during it.
        let mut went_raw = false;
        loop {
            let progress = session.step(wasmux::Budget::syscalls(1)).expect("step");
            went_raw |= session.terminal_raw();
            if matches!(progress, wasmux::Progress::Done(_)) {
                break;
            }
        }
        assert!(
            went_raw,
            "the host was never told the guest wanted raw mode"
        );
    }
}
