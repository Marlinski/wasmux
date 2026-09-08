//! The behaviour corpus: one table of shell cases, and a runner for it.
//!
//! This lives in the library, behind the `corpus` feature, so the same table can be run two
//! ways: by `cargo test` on the host, where the interpreter backend executes it, and by
//! `wasmux-corpus` inside a `wasm32-wasip2` component under wasmtime, where the compiled-in
//! backend does. The two backends must agree, and this is what says so.
//!
//! Every case is ordinary POSIX shell, so a failure is a real difference from what the same
//! script does on Linux and not a difference from something wasmux invented. Keep each case
//! small enough that a failure names one thing.

use crate::{MemVfs, Sandbox};

/// One case: a script, the output it must produce, and the status it must end with.
pub struct Case {
    /// What the case is called, for the failure report.
    pub name: &'static str,
    /// The script, run through `sh -c`.
    pub script: &'static str,
    /// Expected stdout, with trailing whitespace trimmed.
    pub stdout: &'static str,
    /// Expected exit status.
    pub status: i32,
}

const fn case(name: &'static str, script: &'static str, stdout: &'static str) -> Case {
    Case {
        name,
        script,
        stdout,
        status: 0,
    }
}

const fn failing(
    name: &'static str,
    script: &'static str,
    stdout: &'static str,
    status: i32,
) -> Case {
    Case {
        name,
        script,
        stdout,
        status,
    }
}

/// The corpus.
pub static CASES: &[Case] = &[
    // ---- the shell itself ----
    case("echo", "echo hello", "hello"),
    case("arithmetic", "echo $((6 * 7))", "42"),
    case("variables", "x=hello; echo \"${x}-world\" ${#x}", "hello-world 5"),
    case("quoting", "echo 'a  b' \"c  d\"", "a  b c  d"),
    case("command substitution", "echo \"$(echo nested $(echo deep))\"", "nested deep"),
    case("for loop", "for i in 1 2 3; do printf '%s' $i; done", "123"),
    case("while loop", "i=0; while [ $i -lt 3 ]; do i=$((i+1)); done; echo $i", "3"),
    case("if else", "if [ 1 -eq 1 ]; then echo yes; else echo no; fi", "yes"),
    case("case", "case abc in a*) echo matched;; *) echo no;; esac", "matched"),
    case("functions", "f() { echo \"got $1\"; return 3; }; f arg; echo $?", "got arg\n3"),
    case("subshell", "(cd /tmp && pwd); pwd", "/tmp\n/"),
    case("and or", "true && echo a; false || echo b", "a\nb"),
    case("here document", "cat <<EOF\nline one\nline two\nEOF", "line one\nline two"),
    // ---- pipelines and redirection ----
    case("pipeline", "echo one two three | tr ' ' '\\n' | wc -l", "3"),
    case("long pipeline", "seq 1 20 | grep -v 5 | sort -rn | head -3 | tr '\\n' ' '", "20 19 18"),
    case("redirect out and in", "echo saved > /tmp/f; cat < /tmp/f", "saved"),
    case("append", "echo one > /tmp/g; echo two >> /tmp/g; cat /tmp/g", "one\ntwo"),
    case("stderr redirect", "sh -c 'echo oops >&2' 2>/tmp/e; cat /tmp/e", "oops"),
    case("dev null", "echo hidden > /dev/null; echo shown", "shown"),
    case("exec redirect", "exec 3</etc/hosts; read -r line <&3; echo \"$line\"; exec 3<&-", "127.0.0.1 localhost"),
    // ---- exit status ----
    failing("false", "false", "", 1),
    failing("explicit exit", "exit 7", "", 7),
    failing("child status", "sh -c 'exit 5'; echo $?", "5", 0),
    failing("command not found", "/nonexistent 2>/dev/null; exit $?", "", 127),
    // ---- signals ----
    case("trap and kill", "trap 'echo caught' TERM; kill -TERM $$; echo after", "caught\nafter"),
    case("ignored signal", "sh -c 'trap \"\" INT; kill -INT $$; echo survived'", "survived"),
    failing("killed child", "sh -c 'kill -9 $$' 2>/dev/null; echo $?", "Killed\n137", 0),
    // ---- processes ----
    case("background and wait", "sleep 0 & wait; echo done", "done"),
    case("pipe writer exits first", "yes | head -3 | tr '\\n' ' '", "y y y"),
    case("proc self exe", "readlink /proc/self/exe", "/bin/sh"),
    case("many processes", "for i in $(seq 1 20); do true; done; echo ok", "ok"),
    // ---- the filesystem ----
    case("read a mounted file", "cat /data/hello.txt", "from the host"),
    case("write and read back", "echo written > /data/new.txt; cat /data/new.txt", "written"),
    case("mkdir and remove", "mkdir -p /data/a/b && test -d /data/a/b && rmdir /data/a/b && echo ok", "ok"),
    case("find", "mkdir -p /tmp/t && touch /tmp/t/x /tmp/t/y && find /tmp/t -type f | sort | tr '\\n' ' '", "/tmp/t/x /tmp/t/y"),
    case("ls a directory", "ls /data | sort | tr '\\n' ' '", "hello.txt sub"),
    case("read only mount", "echo no > /ro/x 2>/dev/null; echo $?", "1"),
    case("read only readable", "cat /ro/fixed.txt", "immutable"),
    case("rename", "echo v > /tmp/a1; mv /tmp/a1 /tmp/a2; cat /tmp/a2", "v"),
    case("stat size", "printf '12345' > /tmp/s; wc -c < /tmp/s", "5"),
    case("cwd", "cd /data && pwd", "/data"),
    // ---- isolation ----
    case("dot dot cannot escape", "cat ../../../etc/passwd 2>/dev/null; echo $?", "1"),
    case("programs are not readable", "wc -c < /bin/busybox", "0"),
    // ---- the tools ----
    case("sed", "echo abcabc | sed 's/b/B/g'", "aBcaBc"),
    case("awk", "echo '3 4' | awk '{print $1 * $2}'", "12"),
    case("grep", "printf 'a\\nbb\\nccc\\n' | grep -c b", "1"),
    case("sort and uniq", "printf 'b\\na\\nb\\n' | sort | uniq -c | tr -s ' ' | tr '\\n' ' '", " 1 a  2 b"),
    case("cut", "echo 'a:b:c' | cut -d: -f2", "b"),
    case("head and tail", "seq 1 10 | tail -2 | head -1", "9"),
    case("wc", "printf 'a b\\nc\\n' | wc -w", "3"),
    case("basename and dirname", "echo $(basename /a/b/c) $(dirname /a/b/c)", "c /a/b"),
    case("tar round trip", "mkdir -p /tmp/tt && echo packed > /tmp/tt/f && tar cf /tmp/p.tar -C /tmp/tt f && rm /tmp/tt/f && tar xf /tmp/p.tar -C /tmp/tt && cat /tmp/tt/f", "packed"),
    case("gzip round trip", "echo compressed | gzip | gunzip", "compressed"),
    case("jq", "echo '{\"items\":[1,2,3]}' | jq -c '[.items[] | . * 2]'", "[2,4,6]"),
    case("jq reading a file", "jq -r '.name' /data/sub/obj.json", "wasmux"),
    case("shell into jq", "seq 1 4 | jq -s 'add'", "10"),
    // ---- environment ----
    case("environment", "echo $WASMUX_TEST", "set-by-the-host"),
    case("env passes through", "sh -c 'echo $WASMUX_TEST'", "set-by-the-host"),
    case("uname", "uname -s", "Linux"),
    case("shebang script", "printf '#!/bin/sh\\necho from-a-script\\n' > /tmp/x.sh; chmod +x /tmp/x.sh; /tmp/x.sh", "from-a-script"),
];

/// A sandbox with the mounts every case expects.
///
/// Rebuilt for each case, so a case passes on its own rather than because of what ran before.
fn sandbox() -> Result<Sandbox, crate::Error> {
    let data = MemVfs::new()
        .with_file("/hello.txt", b"from the host\n")
        .with_file("/sub/obj.json", br#"{"name":"wasmux","n":1}"#.to_vec());
    let readonly = MemVfs::new().with_file("/fixed.txt", b"immutable\n");
    let etc = MemVfs::new().with_file("/hosts", b"127.0.0.1 localhost\n");
    Sandbox::builder()
        .mount("/", MemVfs::new())
        .mount("/tmp", MemVfs::new())
        .mount("/data", data)
        .mount("/etc", etc)
        .mount_ro("/ro", readonly)
        .env("WASMUX_TEST", "set-by-the-host")
        .build()
}

/// What a corpus run found.
pub struct Report {
    /// How many cases matched.
    pub passed: usize,
    /// One entry per case that did not, formatted for a human.
    pub failures: Vec<String>,
}

impl Report {
    /// Total cases attempted.
    pub fn total(&self) -> usize {
        self.passed.saturating_add(self.failures.len())
    }

    /// Every case matched.
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Run every case, collecting all failures rather than stopping at the first.
pub fn run() -> Report {
    let mut report = Report {
        passed: 0,
        failures: Vec::new(),
    };
    for case in CASES {
        let sandbox = match sandbox() {
            Ok(sandbox) => sandbox,
            Err(e) => {
                report
                    .failures
                    .push(format!("{}\n    sandbox: {e}", case.name));
                continue;
            }
        };
        match sandbox.shell(case.script).output() {
            Ok(out) => {
                let stdout = out.stdout_string().trim_end().to_string();
                let status = out.status.shell_code();
                if stdout == case.stdout && status == case.status {
                    report.passed = report.passed.saturating_add(1);
                } else {
                    report.failures.push(format!(
                        "{}\n    script:   {}\n    expected: {:?} (status {})\n    actual:   {:?} (status {})\n    stderr:   {}",
                        case.name,
                        case.script,
                        case.stdout,
                        case.status,
                        stdout,
                        status,
                        out.stderr_string().trim_end()
                    ));
                }
            }
            Err(e) => report.failures.push(format!(
                "{}\n    script: {}\n    error:  {e}",
                case.name, case.script
            )),
        }
    }
    report
}
