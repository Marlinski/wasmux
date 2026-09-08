//! The first thing to check: that a real program runs at all.
// A test asserts; `unwrap` and `panic!` are how it does that. The library denies them because
// a panic there aborts the consumer's whole component, which is not true of a test binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use wasmux::{MemVfs, Sandbox};

#[test]
fn echo_runs_and_exits_cleanly() {
    let sandbox = Sandbox::builder()
        .mount("/", MemVfs::new())
        .build()
        .expect("build");
    let out = sandbox
        .command("echo")
        .args(["hello", "world"])
        .output()
        .expect("run");
    eprintln!(
        "status={:?} stdout={:?} stderr={:?}",
        out.status,
        out.stdout_string(),
        out.stderr_string()
    );
    assert_eq!(out.stdout_string().trim(), "hello world");
    assert!(out.status.success());
}
