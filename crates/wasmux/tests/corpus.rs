//! Runs the behaviour corpus on this host, where the interpreter backend executes it.
//!
//! The table and the runner live in the library (`wasmux::corpus`) so that the same cases can
//! be run inside a component by `wasmux-corpus`, against the compiled-in backend. A case that
//! passes here and fails there is a backend divergence, which is the thing worth catching.
// A test asserts; `panic!` is how it does that.
#![allow(clippy::panic)]

#[test]
fn corpus() {
    let report = wasmux::corpus::run();
    eprintln!("corpus: {}/{} passed", report.passed, report.total());
    assert!(
        report.is_clean(),
        "{} case(s) failed:\n\n{}\n",
        report.failures.len(),
        report.failures.join("\n\n")
    );
}
