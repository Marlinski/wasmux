//! `wasmux`: run a sandbox from a terminal.
//!
//! For trying the library, reproducing a bug from a shell script, and measuring. It mounts a
//! host directory and runs a command in it, so what you see here is what a consumer of the
//! library gets, minus the terminal.

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};
use wasmux::{Budget, HostVfs, Limits, MemVfs, Output, Progress, Sandbox, Session, Wait};

const USAGE: &str = "\
wasmux: run POSIX programs in a sandbox

usage:
    wasmux [options] [command [args...]]      run a command (default: sh)
    wasmux [options] -c <script>              run a shell script
    wasmux --programs                         list the programs that are available

options:
    -d, --dir <path>       mount this host directory at / (default: an empty memory filesystem)
        --ro <path>        mount this host directory read-only at /ro
    -c <script>            run a shell script
    -e, --env K=V          set an environment variable (repeatable)
        --memory <MiB>     total guest memory limit (default 256)
        --timeout <secs>   wall-clock limit for the whole run (default 60)
        --stream           write output as it appears rather than at the end
        --stats            print syscalls and elapsed time to stderr
    -h, --help             this text

Standard input is passed through, so `echo hi | wasmux -c 'tr a-z A-Z'` works.
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("wasmux: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<u8, String> {
    let mut args = std::env::args().skip(1).peekable();
    let mut dir: Option<String> = None;
    let mut readonly: Option<String> = None;
    let mut script: Option<String> = None;
    let mut env: Vec<(String, String)> = Vec::new();
    let mut memory_mib: u64 = 256;
    let mut timeout = 60u64;
    let mut stream = false;
    let mut stats = false;
    let mut list = false;
    let mut command: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(0);
            }
            "--programs" => list = true,
            "-d" | "--dir" => dir = Some(args.next().ok_or("--dir needs a path")?),
            "--ro" => readonly = Some(args.next().ok_or("--ro needs a path")?),
            "-c" => script = Some(args.next().ok_or("-c needs a script")?),
            "-e" | "--env" => {
                let text = args.next().ok_or("--env needs KEY=VALUE")?;
                let (key, value) = text.split_once('=').ok_or("--env needs KEY=VALUE")?;
                env.push((key.to_string(), value.to_string()));
            }
            "--memory" => {
                memory_mib = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--memory needs a number")?;
            }
            "--timeout" => {
                timeout = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--timeout needs a number")?;
            }
            "--stream" => stream = true,
            "--stats" => stats = true,
            other => {
                command.push(other.to_string());
                command.extend(args.by_ref());
            }
        }
    }

    let limits = Limits {
        memory: memory_mib.saturating_mul(1 << 20),
        memory_per_process: memory_mib.saturating_mul(1 << 20) / 4,
        wall_clock: Some(Duration::from_secs(timeout)),
        ..Limits::default()
    };

    let mut builder = Sandbox::builder()
        .limits(limits)
        .mount("/tmp", MemVfs::new());
    builder = match &dir {
        Some(path) => builder.mount("/", HostVfs::new(path).map_err(|e| format!("{path}: {e}"))?),
        None => builder.mount("/", MemVfs::new()),
    };
    if let Some(path) = &readonly {
        builder = builder.mount_ro(
            "/ro",
            HostVfs::new(path).map_err(|e| format!("{path}: {e}"))?,
        );
    }
    for (key, value) in &env {
        builder = builder.env(key, value);
    }
    let sandbox = builder.build().map_err(|e| e.to_string())?;

    if list {
        for program in sandbox.programs() {
            println!("{:<12} {}", program.name, program.path);
        }
        return Ok(0);
    }

    let prepared = match (&script, command.split_first()) {
        (Some(text), _) => sandbox.shell(text),
        (None, Some((program, rest))) => sandbox.command(program).args(rest.to_vec()),
        (None, None) => sandbox.command("sh"),
    };
    // Standard input is fed only when a program actually asks for it. Reading it up front
    // would block on a pipe that nobody is writing to, which is what an interactive shell
    // looks like from here.
    let prepared = if std::io::stdin().is_terminal() {
        prepared
    } else {
        prepared.interactive_stdin()
    };

    let started = Instant::now();
    let mut session = prepared.spawn().map_err(|e| e.to_string())?;
    let output = drive(&mut session, stream)?;
    if stats {
        eprintln!(
            "wasmux: {} syscalls in {:.3}s{}",
            session.syscall_count(),
            started.elapsed().as_secs_f64(),
            if output.truncated() {
                format!(", {} bytes dropped", output.dropped)
            } else {
                String::new()
            }
        );
    }
    if !stream {
        let _ = std::io::stdout().write_all(&output.stdout);
        let _ = std::io::stderr().write_all(&output.stderr);
    }
    let _ = std::io::stdout().flush();
    Ok(output.status.shell_code().clamp(0, 255) as u8)
}

/// Step the session to completion, optionally writing output as it appears.
fn drive(session: &mut Session, stream: bool) -> Result<Output, String> {
    loop {
        if stream {
            let _ = std::io::stdout().write_all(&session.take_stdout());
            let _ = std::io::stderr().write_all(&session.take_stderr());
        }
        match session
            .step(Budget::syscalls(20_000))
            .map_err(|e| e.to_string())?
        {
            Progress::Done(output) => {
                if stream {
                    let _ = std::io::stdout().write_all(&session.take_stdout());
                    let _ = std::io::stderr().write_all(&session.take_stderr());
                }
                return Ok(output);
            }
            Progress::Yielded => {}
            Progress::Waiting(Wait::Stdin) => {
                let mut chunk = [0u8; 8192];
                match std::io::stdin().read(&mut chunk) {
                    Ok(0) | Err(_) => session.close_stdin(),
                    Ok(n) => session.write_stdin(chunk.get(..n).unwrap_or(&[])),
                }
            }
            Progress::Waiting(Wait::Until(delay)) => {
                std::thread::sleep(delay.min(Duration::from_millis(50)))
            }
            Progress::Waiting(Wait::Vfs | Wait::Host) => {
                // This binary mounts no asynchronous filesystem and registers no host
                // command, so neither can happen; saying so is better than looping forever
                // if one ever does.
                return Err(
                    "something asked to be awaited, which this command cannot do".to_string(),
                );
            }
        }
    }
}
