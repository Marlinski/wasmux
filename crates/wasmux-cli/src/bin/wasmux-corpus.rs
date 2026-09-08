//! Runs the behaviour corpus, in whatever this binary was built for.
//!
//! `cargo test` covers the interpreter backend on the host. This covers the other one: build
//! it for `wasm32-wasip2` and run it under wasmtime, and the same table executes against the
//! programs compiled into the component. A case that passes on one backend and fails on the
//! other is the divergence worth catching, and nothing else finds it.
//!
//! ```text
//! cargo run -p wasmux-cli --bin wasmux-corpus
//! make test-wasm
//! ```
//!
//! Exits non-zero if any case failed, so it works as a CI step.

fn main() {
    let backend = if cfg!(target_arch = "wasm32") {
        "compiled in (wasm2c)"
    } else {
        "interpreter (wasmi)"
    };
    let report = wasmux::corpus::run();
    println!(
        "corpus on {backend}: {}/{} passed",
        report.passed,
        report.total()
    );

    if report.is_clean() {
        return;
    }
    println!();
    for failure in &report.failures {
        println!("{failure}\n");
    }
    println!("{} case(s) failed", report.failures.len());
    std::process::exit(1);
}
