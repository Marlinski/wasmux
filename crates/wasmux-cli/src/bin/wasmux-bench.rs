//! Measures what wasmux costs, on the workloads an agent's shell tool actually runs.
//!
//! Deliberately not a microbenchmark harness: these are whole commands, timed end to end,
//! because that is the number an integrator has to plan around. Run it the same way on a
//! native build and on a `wasm32-wasip2` build under wasmtime to compare the two backends.
//!
//! ```text
//! cargo run --release -p wasmux-cli --bin wasmux-bench
//! ```

use std::time::{Duration, Instant};
use wasmux::{MemVfs, Sandbox};

struct Bench {
    name: &'static str,
    script: &'static str,
    /// How many times to run it. Short ones are repeated so the number means something.
    runs: u32,
}

const BENCHES: &[Bench] = &[
    Bench { name: "startup (true)", script: "true", runs: 20 },
    Bench { name: "echo", script: "echo hello", runs: 20 },
    Bench { name: "spawn 20 processes", script: "for i in $(seq 1 20); do /bin/true; done", runs: 5 },
    Bench { name: "pipeline, 4 stages, 2k lines", script: "seq 1 2000 | grep 7 | sort -r | wc -l", runs: 5 },
    Bench { name: "sed over 5k lines", script: "seq 1 5000 | sed 's/[0-9]*/n/' | tail -1", runs: 5 },
    Bench { name: "awk sum to 20k", script: "awk 'BEGIN{s=0; for(i=0;i<20000;i++) s+=i; print s}'", runs: 5 },
    Bench { name: "jq over 2k numbers", script: "seq 1 2000 | jq -s 'add'", runs: 5 },
    Bench { name: "write and read 1 MiB", script: "dd if=/dev/zero bs=1024 count=1024 2>/dev/null > /tmp/b; wc -c < /tmp/b", runs: 5 },
    Bench { name: "find over 200 files", script: "mkdir -p /tmp/d && for i in $(seq 1 200); do echo x > /tmp/d/f$i; done && find /tmp/d -type f | wc -l", runs: 3 },
];

fn main() {
    let backend = if cfg!(target_arch = "wasm32") {
        "compiled in (wasm2c)"
    } else {
        "interpreter (wasmi)"
    };
    println!("wasmux benchmark: {backend}\n");
    println!(
        "{:<32} {:>10} {:>10} {:>12}",
        "workload", "median", "best", "syscalls"
    );
    println!("{}", "-".repeat(68));

    let mut total = Duration::ZERO;
    for bench in BENCHES {
        let mut timings: Vec<Duration> = Vec::new();
        let mut syscalls = 0;
        for _ in 0..bench.runs {
            // A fresh sandbox each run: this measures the cost an agent pays per tool call,
            // including building the process, not just the steady state.
            let sandbox = Sandbox::builder()
                .mount("/", MemVfs::new())
                .mount("/tmp", MemVfs::new())
                .build()
                .expect("the sandbox should build");
            let started = Instant::now();
            let mut session = sandbox.shell(bench.script).spawn().expect("spawn");
            let output = loop {
                match session.step(wasmux::Budget::unlimited()).expect("step") {
                    wasmux::Progress::Done(output) => break output,
                    wasmux::Progress::Yielded => {}
                    wasmux::Progress::Waiting(_) => session.close_stdin(),
                }
            };
            timings.push(started.elapsed());
            syscalls = session.syscall_count();
            assert!(
                output.status.success(),
                "{} failed: {}",
                bench.name,
                output.stderr_string()
            );
        }
        timings.sort();
        let median = timings.get(timings.len() / 2).copied().unwrap_or_default();
        let best = timings.first().copied().unwrap_or_default();
        total = total.saturating_add(median);
        println!(
            "{:<32} {:>9.1}ms {:>9.1}ms {:>12}",
            bench.name,
            median.as_secs_f64() * 1000.0,
            best.as_secs_f64() * 1000.0,
            syscalls
        );
    }
    println!("{}", "-".repeat(68));
    println!(
        "{:<32} {:>9.1}ms",
        "total (median)",
        total.as_secs_f64() * 1000.0
    );
}
