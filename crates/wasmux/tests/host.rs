//! What a [`HostCommand`] must behave like: a program, in every respect a guest can observe.
//!
//! The point of the feature is that an embedder writes a function and gets a process. These
//! cases are the specific claims that makes — it pipes, it redirects, it is captured by
//! `$( )`, its status reaches `$?`, it can be suspended — because each of them is somewhere
//! the implementation could plausibly have cut a corner and none of them are obvious from
//! reading it.
// A test asserts; `unwrap` and `panic!` are how it does that.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use wasmux::{Budget, Errno, Exit, HostCommand, Invocation, MemVfs, Progress, Sandbox, VfsResult};

/// Echoes its arguments, so argv arrives intact.
struct Args;

impl HostCommand for Args {
    fn run(&self, call: &Invocation) -> VfsResult<Exit> {
        Ok(Exit::from_stdout(call.args()[1..].join("|") + "\n"))
    }
}

impl HostCommand for Fails {
    fn run(&self, _: &Invocation) -> VfsResult<Exit> {
        Ok(Exit::failed(3, "it went wrong\n"))
    }
}

/// Exits non-zero with a message on standard error.
struct Fails;

/// Says `AGAIN` a fixed number of times before answering, the way an embedder awaiting an
/// HTTP response would.
struct Slow {
    remaining: AtomicU32,
    calls: Arc<AtomicU32>,
}

impl HostCommand for Slow {
    fn run(&self, _: &Invocation) -> VfsResult<Exit> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.remaining.fetch_sub(1, Ordering::Relaxed) > 1 {
            return Err(Errno::AGAIN);
        }
        Ok(Exit::from_stdout("eventually\n"))
    }
}

/// Reports what it was given, to check the retry is identical.
struct EchoStdinOnce {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    first: AtomicU32,
}

impl HostCommand for EchoStdinOnce {
    fn reads_stdin(&self, _: &[Vec<u8>]) -> bool {
        true
    }

    fn run(&self, call: &Invocation) -> VfsResult<Exit> {
        self.seen
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(&call.stdin).into_owned());
        if self.first.fetch_add(1, Ordering::Relaxed) == 0 {
            return Err(Errno::AGAIN);
        }
        Ok(Exit::from_stdout(call.stdin.clone()))
    }
}

/// Upper-cases its standard input, so it works as a filter.
struct Upper;

impl HostCommand for Upper {
    fn run(&self, call: &Invocation) -> VfsResult<Exit> {
        Ok(Exit::from_stdout(
            String::from_utf8_lossy(&call.stdin).to_uppercase(),
        ))
    }

    fn reads_stdin(&self, _: &[Vec<u8>]) -> bool {
        true
    }
}

fn sandbox() -> Sandbox {
    Sandbox::builder()
        .mount("/", MemVfs::new())
        .mount("/tmp", MemVfs::new())
        .command("args", Args)
        .command("upper", Upper)
        .command("fails", Fails)
        .build()
        .expect("the sandbox should build")
}

fn run(script: &str) -> wasmux::Output {
    sandbox()
        .shell(script)
        .output()
        .expect("the script should run")
}

#[test]
fn appears_as_a_program() {
    let sandbox = sandbox();
    assert!(sandbox.has_program("args"));
    let listed = sandbox.programs();
    let entry = listed.iter().find(|p| p.name == "args").unwrap();
    assert_eq!(entry.path, "/usr/bin/args");

    // And the guest agrees: `ls`, `which` and `test -x` all go through path resolution.
    let out = run("ls /usr/bin/args && test -x /usr/bin/args && echo yes");
    assert_eq!(out.stdout_string().trim(), "/usr/bin/args\nyes");
}

#[test]
fn receives_argv_and_the_environment() {
    let out = run("args one two 'three four'");
    assert_eq!(out.stdout_string().trim(), "one|two|three four");
}

#[test]
fn reads_the_environment() {
    struct Env;
    impl HostCommand for Env {
        fn run(&self, call: &Invocation) -> VfsResult<Exit> {
            Ok(Exit::from_stdout(format!(
                "{}\n",
                call.env("PICKED").unwrap_or_default()
            )))
        }
    }
    let out = Sandbox::builder()
        .mount("/", MemVfs::new())
        .env("PICKED", "from-the-host")
        .command("showenv", Env)
        .build()
        .unwrap()
        .shell("showenv")
        .output()
        .unwrap();
    assert_eq!(out.stdout_string().trim(), "from-the-host");
}

#[test]
fn works_in_the_middle_of_a_pipeline() {
    // A guest, then a host command, then a guest again: the bytes have to cross both seams.
    let out = run("printf 'ab\\ncd\\n' | upper | sort -r");
    assert_eq!(out.stdout_string().trim(), "CD\nAB");
}

#[test]
fn output_can_be_redirected_and_read_back() {
    let out = run("args x y > /tmp/o.txt; wc -c < /tmp/o.txt");
    assert_eq!(out.stdout_string().trim(), "4");
}

#[test]
fn is_captured_by_command_substitution() {
    let out = run("echo \"[$(args a b)]\"");
    assert_eq!(out.stdout_string().trim(), "[a|b]");
}

#[test]
fn its_status_reaches_the_shell() {
    let out = run("fails; echo status=$?");
    assert_eq!(out.stdout_string().trim(), "status=3");
    assert_eq!(out.stderr_string(), "it went wrong\n");

    // And it participates in `&&` / `||` like anything else.
    let out = run("fails || echo recovered");
    assert_eq!(out.stdout_string().trim(), "recovered");
}

#[test]
fn a_command_that_does_not_read_stdin_does_not_wait_for_it() {
    // The failure this guards against: `args` as the first stage of a pipeline blocking on a
    // standard input nothing will ever close, because the kernel drained it unasked.
    let mut session = sandbox().shell("args solo").spawn().unwrap();
    let mut steps = 0;
    loop {
        steps += 1;
        assert!(steps < 10_000, "it should not spin");
        match session.step(Budget::unlimited()).unwrap() {
            Progress::Done(out) => {
                assert_eq!(out.stdout_string().trim(), "solo");
                return;
            }
            Progress::Yielded => {}
            Progress::Waiting(w) => panic!("it should not have waited: {w:?}"),
        }
    }
}

#[test]
fn again_suspends_and_the_retry_is_identical() {
    let calls = Arc::new(AtomicU32::new(0));
    let sandbox = Sandbox::builder()
        .mount("/", MemVfs::new())
        .command(
            "slow",
            Slow {
                remaining: AtomicU32::new(3),
                calls: calls.clone(),
            },
        )
        .build()
        .unwrap();

    let mut session = sandbox.shell("slow").spawn().unwrap();
    let mut waits = 0;
    let out = loop {
        match session.step(Budget::unlimited()).unwrap() {
            Progress::Done(out) => break out,
            Progress::Yielded => {}
            Progress::Waiting(wasmux::Wait::Host) => waits += 1,
            Progress::Waiting(w) => panic!("unexpected wait: {w:?}"),
        }
    };
    assert_eq!(out.stdout_string().trim(), "eventually");
    assert_eq!(calls.load(Ordering::Relaxed), 3, "it was retried");
    assert_eq!(waits, 2, "each AGAIN surfaced as Wait::Host");
}

#[test]
fn a_suspended_command_keeps_the_stdin_it_was_given() {
    // The contract says the retry gets the identical `Invocation`. If the kernel handed the
    // buffer away on the first call, the second would see an empty standard input, and a
    // `curl -d @-` would silently post nothing.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sandbox = Sandbox::builder()
        .mount("/", MemVfs::new())
        .command_at(
            "consume",
            "/usr/bin/consume",
            EchoStdinOnce {
                seen: seen.clone(),
                first: AtomicU32::new(0),
            },
        )
        .build()
        .unwrap();

    let mut session = sandbox.shell("printf 'payload' | consume").spawn().unwrap();
    let out = loop {
        if let Progress::Done(out) = session.step(Budget::unlimited()).unwrap() {
            break out;
        }
    };
    assert_eq!(out.stdout_string(), "payload");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "it ran twice");
    assert_eq!(seen[0], "payload");
    assert_eq!(seen[1], "payload", "the retry saw the same input");
}

#[test]
fn a_host_command_replaces_an_applet_of_the_same_name() {
    // The documented way to override a shipped tool.
    struct FakeEcho;
    impl HostCommand for FakeEcho {
        fn run(&self, _: &Invocation) -> VfsResult<Exit> {
            Ok(Exit::from_stdout("not the real echo\n"))
        }
    }
    let out = Sandbox::builder()
        .mount("/", MemVfs::new())
        .command_at("echo", "/bin/echo", FakeEcho)
        .build()
        .unwrap()
        .shell("/bin/echo hello")
        .output()
        .unwrap();
    assert_eq!(out.stdout_string().trim(), "not the real echo");
}

#[test]
fn a_command_that_needs_stdin_gets_all_of_it() {
    // More than one pipe buffer's worth, so the read loop has to go round.
    let out = run("seq 1 5000 | upper | wc -c");
    let expected: usize = (1..=5000).map(|n| n.to_string().len() + 1).sum();
    assert_eq!(out.stdout_string().trim(), expected.to_string());
}

#[test]
fn several_host_commands_run_in_one_pipeline() {
    let out = run("args a b | upper | upper");
    assert_eq!(out.stdout_string().trim(), "A|B");
}

#[test]
fn a_failing_command_does_not_take_the_sandbox_down() {
    struct Boom;
    impl HostCommand for Boom {
        fn run(&self, _: &Invocation) -> VfsResult<Exit> {
            // Not `AGAIN`: a genuine error from trying to run it at all.
            Err(Errno::IO)
        }
    }
    let sandbox = Sandbox::builder()
        .mount("/", MemVfs::new())
        .command("boom", Boom)
        .build()
        .unwrap();
    let out = sandbox.shell("boom; echo still here").output().unwrap();
    assert_eq!(out.stdout_string().trim(), "still here");
    assert!(
        out.stderr_string().contains("boom"),
        "the reason should name the command: {:?}",
        out.stderr_string()
    );
}
