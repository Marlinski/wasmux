//! What a long session must not run out of.
//!
//! An agent keeps one sandbox for a conversation and runs script after script in it, so
//! anything a process takes and does not give back is lost for the rest of that conversation.
// A test asserts; `unwrap` and `panic!` are how it does that. The library denies them because
// a panic there aborts the consumer's whole component, which is not true of a test binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use wasmux::{Limits, MemVfs, Sandbox};

fn sandbox(limits: Limits) -> Sandbox {
    Sandbox::builder()
        .mount("/", MemVfs::new())
        .mount("/tmp", MemVfs::new())
        .limits(limits)
        .build()
        .expect("build")
}

/// Every heap a program grew used to stay charged to the session after it exited, so a loop
/// of short `jq` calls — six per item in a script fetching a news feed — spent the whole
/// budget in about fourteen runs, and every program after that failed to start.
#[test]
fn programs_that_exit_give_their_memory_back() {
    let out = sandbox(Limits::default())
        .shell("for i in $(seq 200); do echo '{}' | jq . | cat >/dev/null; done; echo done")
        .output()
        .expect("run");
    assert_eq!(
        out.stdout_string().trim(),
        "done",
        "stderr: {:?}",
        out.stderr_string()
    );
    assert!(out.status.success(), "status {:?}", out.status);
}

/// BusyBox runs `$( )` and every pipeline stage by re-executing the shell with the subshell's
/// variables in its arguments, so a variable is only as long as one argument may be. That was
/// a path's length, 4 KiB: one `curl` response kept in a variable broke the next pipeline.
#[test]
fn a_variable_over_four_kilobytes_crosses_a_pipeline() {
    let out = sandbox(Limits::default())
        .shell(
            r#"BIG=$(head -c 20000 /dev/zero | tr '\0' x)
               echo "$BIG" | wc -c
               N=$(echo "$BIG" | wc -c); echo "n=$N""#,
        )
        .output()
        .expect("run");
    assert_eq!(
        out.stdout_string().split_whitespace().collect::<Vec<_>>(),
        ["20001", "n=20001"],
        "stderr: {:?}",
        out.stderr_string()
    );
}

/// A program that cannot be started still holds the descriptors it was given, and the write
/// end of a pipe kept by a process that never ran is one its reader waits on forever. That
/// used to end the whole session as a deadlock rather than with the reader seeing end of file.
///
/// Four megabytes is enough for the shell and too little for `jq`, so `jq` fails to start.
#[test]
fn a_program_that_cannot_start_still_closes_its_pipe() {
    let out = sandbox(Limits {
        memory: 4 << 20,
        ..Limits::default()
    })
    .shell("jq -n 1 | cat; V=$(jq -n 2); echo \"end$V\"")
    .output()
    .expect("the session must end, not deadlock");
    assert_eq!(
        out.stdout_string().trim(),
        "end",
        "stderr: {:?}",
        out.stderr_string()
    );
}
