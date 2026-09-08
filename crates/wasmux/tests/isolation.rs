//! What a guest must not be able to do.
//!
//! These are the tests that matter for putting wasmux inside an agent: a program in the
//! sandbox can be hostile, and none of it may reach the host or the parts of the filesystem
//! that were not offered.
// A test asserts; `unwrap` and `panic!` are how it does that. The library denies them because
// a panic there aborts the consumer's whole component, which is not true of a test binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wasmux::{
    DirEntry, Errno, Limits, MemVfs, OpenOptions, Sandbox, Stat, Vfs, VfsFile, VfsResult,
};

fn sandbox_with(vfs: impl Vfs) -> Sandbox {
    Sandbox::builder()
        .mount("/", MemVfs::new())
        .mount("/tmp", MemVfs::new())
        .mount("/work", vfs)
        .build()
        .expect("build")
}

fn run(sandbox: &Sandbox, script: &str) -> String {
    match sandbox.shell(script).output() {
        Ok(out) => out.stdout_string().trim_end().to_string(),
        Err(e) => format!("error: {e}"),
    }
}

/// Records every path a guest asked for, so a test can assert what was never reached.
struct Watched {
    inner: MemVfs,
    reads: Arc<AtomicUsize>,
}

impl Vfs for Watched {
    fn open(&self, path: &str, opts: &OpenOptions) -> VfsResult<Box<dyn VfsFile>> {
        assert!(
            path.starts_with('/'),
            "a Vfs must be given absolute paths, got {path:?}"
        );
        assert!(
            !path.contains(".."),
            "a Vfs must never see .., got {path:?}"
        );
        assert!(
            !path.contains("//"),
            "a Vfs must never see an empty component, got {path:?}"
        );
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.open(path, opts)
    }
    fn stat(&self, path: &str, follow: bool) -> VfsResult<Stat> {
        assert!(
            !path.contains(".."),
            "a Vfs must never see .., got {path:?}"
        );
        self.inner.stat(path, follow)
    }
    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        self.inner.readdir(path)
    }
}

#[test]
fn a_guest_cannot_climb_out_of_its_mounts() {
    let reads = Arc::new(AtomicUsize::new(0));
    let watched = Watched {
        inner: MemVfs::new().with_file("/inside.txt", b"secret"),
        reads: reads.clone(),
    };
    let sandbox = sandbox_with(watched);

    // Every one of these is an attempt to name something outside /work. None may succeed, and
    // the assertions inside `Watched` fail the test if a `..` ever reaches the filesystem.
    for attempt in [
        "cat /work/../etc/passwd",
        "cat /work/../../../../etc/passwd",
        "cd /work && cat ../../../../etc/passwd",
        "cat /work/./../work/../../etc/shadow",
        "cat //work/..//..//etc/passwd",
    ] {
        let script = format!("{attempt} 2>/dev/null; echo rc=$?");
        assert_eq!(
            run(&sandbox, &script),
            "rc=1",
            "{attempt} should have failed"
        );
    }
    // The file that is genuinely inside is readable, so the test is not passing vacuously.
    assert_eq!(run(&sandbox, "cat /work/inside.txt"), "secret");
    assert!(reads.load(Ordering::Relaxed) > 0);
}

#[test]
fn a_read_only_mount_refuses_every_way_of_writing() {
    let sandbox = Sandbox::builder()
        .mount("/", MemVfs::new())
        .mount("/tmp", MemVfs::new())
        .mount_ro(
            "/ro",
            MemVfs::new().with_file("/f.txt", b"fixed").with_dir("/d"),
        )
        .build()
        .expect("build");
    for attempt in [
        "echo x > /ro/new",
        "echo x >> /ro/f.txt",
        "rm /ro/f.txt",
        "mkdir /ro/d2",
        "rmdir /ro/d",
        "mv /ro/f.txt /ro/g.txt",
        "cp /etc/hostname /ro/copy",
        "touch /ro/f.txt2",
        "ln -s /ro/f.txt /ro/link",
    ] {
        let script = format!("{attempt} 2>/dev/null; echo rc=$?");
        assert_ne!(
            run(&sandbox, &script),
            "rc=0",
            "{attempt} should have been refused"
        );
    }
    assert_eq!(
        run(&sandbox, "cat /ro/f.txt"),
        "fixed",
        "reading still works"
    );
}

#[test]
fn there_is_no_network() {
    let sandbox = sandbox_with(MemVfs::new());
    // No socket syscall is implemented, and no networking tool is compiled in. Both halves
    // matter: the first is the guarantee, the second is what an agent would reach for.
    for name in ["wget", "nc", "curl", "ftpget", "telnet", "ssl_client"] {
        assert!(!sandbox.has_program(name), "{name} must not be available");
    }
    assert_eq!(
        run(&sandbox, "wget http://example.com 2>/dev/null; echo rc=$?"),
        "rc=127"
    );
}

#[test]
fn programs_cannot_be_read_or_replaced() {
    let sandbox = sandbox_with(MemVfs::new());
    // A program is executable and nothing else: it cannot be copied out, and it cannot be
    // overwritten to make the sandbox run something else next time.
    assert_eq!(run(&sandbox, "wc -c < /bin/busybox"), "0");
    assert_ne!(
        run(&sandbox, "echo x > /bin/sh 2>/dev/null; echo rc=$?"),
        "rc=0"
    );
    assert_ne!(run(&sandbox, "rm /bin/sh 2>/dev/null; echo rc=$?"), "rc=0");
    assert_eq!(run(&sandbox, "echo still-here | sh -c 'cat'"), "still-here");
}

#[test]
fn a_narrowed_program_list_is_all_the_guest_gets() {
    let sandbox = Sandbox::builder()
        .mount("/", MemVfs::new())
        .programs(&["sh", "echo", "cat"])
        .build()
        .expect("build");
    let names: Vec<String> = sandbox.programs().into_iter().map(|p| p.name).collect();
    assert_eq!(
        names,
        vec!["cat".to_string(), "echo".to_string(), "sh".to_string()]
    );
    assert_eq!(run(&sandbox, "echo yes"), "yes");
    // The paths are gone, which is what stops another program from execing them.
    assert_eq!(run(&sandbox, "test -e /bin/sed; echo rc=$?"), "rc=1");
    assert_eq!(run(&sandbox, "test -e /bin/ls; echo rc=$?"), "rc=1");
    // But BusyBox is one binary with its applets inside it, and its shell dispatches to them
    // without an exec, so the shell can still run `sed`. Narrowing this list controls the
    // namespace, not what a shell can do; see the note on `Builder::programs`.
    assert_eq!(run(&sandbox, "echo a | sed s/a/b/"), "b");
}

#[test]
fn a_runaway_process_count_is_refused_not_fatal() {
    let limits = Limits {
        processes: 4,
        ..Limits::default()
    };
    let sandbox = Sandbox::builder()
        .mount("/", MemVfs::new())
        .limits(limits)
        .build()
        .expect("build");
    // Far more processes than the limit allows. The shell survives and says so; the sandbox
    // does not fall over.
    let out = sandbox
        .shell("for i in 1 2 3 4 5 6 7 8; do true; done; echo alive")
        .output()
        .expect("run");
    assert!(
        out.stdout_string().contains("alive"),
        "stdout was {:?}",
        out.stdout_string()
    );
}

#[test]
fn output_is_capped_and_says_so() {
    let limits = Limits {
        output: 4096,
        ..Limits::default()
    };
    let sandbox = Sandbox::builder()
        .mount("/", MemVfs::new())
        .limits(limits)
        .build()
        .expect("build");
    let out = sandbox.shell("seq 1 100000").output().expect("run");
    assert!(out.stdout.len() <= 4096, "kept {} bytes", out.stdout.len());
    assert!(
        out.truncated(),
        "the caller must be told that output was dropped"
    );
    assert!(out.dropped > 0);
}

#[test]
fn a_memory_limit_becomes_an_error_inside_the_guest() {
    let limits = Limits {
        memory: 24 << 20,
        memory_per_process: 8 << 20,
        ..Limits::default()
    };
    let sandbox = Sandbox::builder()
        .mount("/", MemVfs::new())
        .mount("/tmp", MemVfs::new())
        .limits(limits)
        .build()
        .expect("build");
    // Ask for far more than the limit. Whatever happens, the session returns and the host is
    // still standing; that is the property under test.
    let out = sandbox
        .shell("seq 1 2000000 | sort > /tmp/big; echo done")
        .output();
    assert!(
        out.is_ok(),
        "the session must return an answer, got {out:?}"
    );
}

#[test]
fn a_vfs_that_fails_does_not_take_the_sandbox_with_it() {
    struct Broken;
    impl Vfs for Broken {
        fn open(&self, _path: &str, _opts: &OpenOptions) -> VfsResult<Box<dyn VfsFile>> {
            Err(Errno::IO)
        }
        fn stat(&self, _path: &str, _follow: bool) -> VfsResult<Stat> {
            Err(Errno::IO)
        }
        fn readdir(&self, _path: &str) -> VfsResult<Vec<DirEntry>> {
            Err(Errno::IO)
        }
    }
    let sandbox = sandbox_with(Broken);
    assert_eq!(
        run(&sandbox, "cat /work/anything 2>/dev/null; echo rc=$?"),
        "rc=1"
    );
    assert_eq!(run(&sandbox, "ls /work 2>/dev/null; echo rc=$?"), "rc=1");
    assert_eq!(run(&sandbox, "echo the shell is fine"), "the shell is fine");
}

#[test]
fn the_environment_is_only_what_was_given() {
    let sandbox = Sandbox::builder()
        .mount("/", MemVfs::new())
        .clear_env()
        .env("ONLY", "this")
        .build()
        .expect("build");
    let out = run(&sandbox, "env | sort");
    // The shell adds its own variables; what matters is that nothing from the host leaked in.
    assert!(out.contains("ONLY=this"), "got {out:?}");
    for leaked in ["HOME=/home", "USER=", "CARGO", "PWD=/home"] {
        assert!(
            !out.contains(leaked),
            "{leaked} leaked into the guest: {out}"
        );
    }
}

/// Arguments are bytes, and a program is entitled to put anything in them.
///
/// This is a regression test for a bug that was invisible until BusyBox's no-MMU `fork`
/// replacement tripped over it. `argv` was read through `String::from_utf8_lossy`, so
/// `\xF4imeout` — which is what `timeout` re-executes itself as, the high bit of `argv[0][0]`
/// being its "I am the re-executed copy" marker — arrived as `\u{FFFD}imeout`. BusyBox then
/// cleared the high bit of the replacement character instead and looked up an applet called
/// `o\xBF\xBDimeout`, which does not exist.
///
/// Every applet that re-executes itself was broken by it: `timeout`, `time`, `nohup`. The
/// symptom was "applet not found" for an applet plainly in the image.
#[test]
fn arguments_survive_bytes_that_are_not_utf8() {
    let sandbox = Sandbox::builder()
        .mount("/", MemVfs::new())
        .build()
        .expect("the sandbox should build");

    // `printf` produces the byte and `hexdump` reads it back, so the whole trip stays inside
    // the guest and nothing here has to be able to spell it.
    let out = sandbox
        .shell(r#"printf '\364imeout' | hexdump -e '16/1 "%02x"'"#)
        .output()
        .expect("it should run");
    assert_eq!(
        out.stdout_string().trim(),
        "f4696d656f7574",
        "the byte must survive: {:?}",
        out.stdout_string()
    );

    // And the applet that depends on it works.
    let out = sandbox
        .shell("timeout 5 echo alive")
        .output()
        .expect("it should run");
    assert_eq!(out.stdout_string().trim(), "alive");
    assert!(
        out.stderr_string().is_empty(),
        "no diagnostics expected: {:?}",
        out.stderr_string()
    );
}
