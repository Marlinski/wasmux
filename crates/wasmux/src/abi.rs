//! Linux ABI constants for the `wasm32-linux` userland: asm-generic numbering, 32-bit, time64.
//!
//! These are the values musl was compiled against; see `docs/ABI.md`.
#![allow(dead_code)]

// open(2)
pub const O_ACCMODE: u32 = 3;
pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_CREAT: u32 = 0o100;
pub const O_EXCL: u32 = 0o200;
pub const O_NOCTTY: u32 = 0o400;
pub const O_TRUNC: u32 = 0o1000;
pub const O_APPEND: u32 = 0o2000;
pub const O_NONBLOCK: u32 = 0o4000;
pub const O_DIRECTORY: u32 = 0o200000;
pub const O_NOFOLLOW: u32 = 0o400000;
pub const O_CLOEXEC: u32 = 0o2000000;
pub const O_PATH: u32 = 0o10000000;

pub const AT_FDCWD: i32 = -100;
pub const AT_SYMLINK_NOFOLLOW: i32 = 0x100;
pub const AT_REMOVEDIR: i32 = 0x200;
pub const AT_SYMLINK_FOLLOW: i32 = 0x400;
pub const AT_EMPTY_PATH: i32 = 0x1000;

// fcntl
pub const F_DUPFD: i32 = 0;
pub const F_GETFD: i32 = 1;
pub const F_SETFD: i32 = 2;
pub const F_GETFL: i32 = 3;
pub const F_SETFL: i32 = 4;
pub const F_GETLK: i32 = 5;
pub const F_SETLK: i32 = 6;
pub const F_SETLKW: i32 = 7;
pub const F_DUPFD_CLOEXEC: i32 = 1030;
pub const FD_CLOEXEC: i32 = 1;

// lseek
pub const SEEK_SET: i32 = 0;
pub const SEEK_CUR: i32 = 1;
pub const SEEK_END: i32 = 2;

// mmap
pub const MAP_SHARED: i32 = 1;
pub const MAP_PRIVATE: i32 = 2;
pub const MAP_FIXED: i32 = 0x10;
pub const MAP_ANONYMOUS: i32 = 0x20;

// clone
pub const CLONE_VM: u32 = 0x100;
pub const CLONE_VFORK: u32 = 0x4000;
pub const CLONE_THREAD: u32 = 0x10000;

// wait4
pub const WNOHANG: i32 = 1;
pub const WUNTRACED: i32 = 2;

// signals
pub const SIGHUP: i32 = 1;
pub const SIGINT: i32 = 2;
pub const SIGQUIT: i32 = 3;
pub const SIGILL: i32 = 4;
pub const SIGTRAP: i32 = 5;
pub const SIGABRT: i32 = 6;
pub const SIGBUS: i32 = 7;
pub const SIGFPE: i32 = 8;
pub const SIGKILL: i32 = 9;
pub const SIGUSR1: i32 = 10;
pub const SIGSEGV: i32 = 11;
pub const SIGUSR2: i32 = 12;
pub const SIGPIPE: i32 = 13;
pub const SIGALRM: i32 = 14;
pub const SIGTERM: i32 = 15;
pub const SIGCHLD: i32 = 17;
pub const SIGCONT: i32 = 18;
pub const SIGSTOP: i32 = 19;
pub const SIGTSTP: i32 = 20;
pub const SIGTTIN: i32 = 21;
pub const SIGTTOU: i32 = 22;
pub const SIGURG: i32 = 23;
pub const SIGWINCH: i32 = 28;
pub const SIG_DFL: u32 = 0;
pub const SIG_IGN: u32 = 1;
pub const SA_SIGINFO: u32 = 4;
pub const SA_NODEFER: u32 = 0x40000000;
pub const SA_RESETHAND: u32 = 0x80000000;
pub const SIG_BLOCK: i32 = 0;
pub const SIG_UNBLOCK: i32 = 1;
pub const SIG_SETMASK: i32 = 2;

// ioctl
pub const TCGETS: u32 = 0x5401;
pub const TCSETS: u32 = 0x5402;
pub const TCSETSW: u32 = 0x5403;
pub const TCSETSF: u32 = 0x5404;
pub const TIOCGPGRP: u32 = 0x540F;
pub const TIOCSPGRP: u32 = 0x5410;
pub const TIOCGWINSZ: u32 = 0x5413;
pub const TIOCSWINSZ: u32 = 0x5414;
pub const FIONREAD: u32 = 0x541B;
pub const TIOCSCTTY: u32 = 0x540E;
pub const TIOCNOTTY: u32 = 0x5422;
pub const FIONBIO: u32 = 0x5421;
pub const FIOCLEX: u32 = 0x5451;
pub const FIONCLEX: u32 = 0x5450;

// termios flags
pub const ICRNL: u32 = 0o400;
pub const IXON: u32 = 0o2000;
pub const OPOST: u32 = 1;
pub const ONLCR: u32 = 4;
pub const ISIG: u32 = 1;
pub const ICANON: u32 = 2;
pub const ECHO: u32 = 8;
pub const ECHOE: u32 = 0o20;
pub const ECHOK: u32 = 0o40;
pub const ECHONL: u32 = 0o100;
pub const IEXTEN: u32 = 0o100000;
pub const VINTR: usize = 0;
pub const VQUIT: usize = 1;
pub const VERASE: usize = 2;
pub const VKILL: usize = 3;
pub const VEOF: usize = 4;
pub const VTIME: usize = 5;
pub const VMIN: usize = 6;
pub const VSUSP: usize = 10;
pub const VEOL: usize = 11;
pub const VWERASE: usize = 14;
pub const NCCS_KERNEL: usize = 19;
pub const CS8: u32 = 0o60;
pub const CREAD: u32 = 0o200;
pub const B38400: u32 = 0o17;

// poll
pub const POLLIN: i16 = 1;
pub const POLLPRI: i16 = 2;
pub const POLLOUT: i16 = 4;
pub const POLLERR: i16 = 8;
pub const POLLHUP: i16 = 0x10;
pub const POLLNVAL: i16 = 0x20;

// auxv
pub const AT_NULL: u32 = 0;
pub const AT_PAGESZ: u32 = 6;
pub const AT_UID: u32 = 11;
pub const AT_EUID: u32 = 12;
pub const AT_GID: u32 = 13;
pub const AT_EGID: u32 = 14;
pub const AT_CLKTCK: u32 = 17;
pub const AT_SECURE: u32 = 23;
pub const AT_RANDOM: u32 = 25;
pub const AT_EXECFN: u32 = 31;

// stat mode bits
pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFIFO: u32 = 0o010000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFSOCK: u32 = 0o140000;

// dirent d_type
pub const DT_UNKNOWN: u8 = 0;
pub const DT_FIFO: u8 = 1;
pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_BLK: u8 = 6;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;
pub const DT_SOCK: u8 = 12;

// clocks
pub const CLOCK_REALTIME: i32 = 0;
pub const CLOCK_MONOTONIC: i32 = 1;
pub const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
pub const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
pub const CLOCK_BOOTTIME: i32 = 7;

// rlimits
pub const RLIMIT_CPU: i32 = 0;
pub const RLIMIT_FSIZE: i32 = 1;
pub const RLIMIT_DATA: i32 = 2;
pub const RLIMIT_STACK: i32 = 3;
pub const RLIMIT_CORE: i32 = 4;
pub const RLIMIT_NOFILE: i32 = 7;
pub const RLIM_INFINITY: u64 = !0;

pub const WASM_PAGE: u64 = 65536;

// statx
/// The mask `statx` reports as filled in.
pub const STATX_BASIC_STATS: u32 = 0x7ff;

// clock_nanosleep
/// The deadline is absolute rather than a duration.
pub const TIMER_ABSTIME: i32 = 1;

/// Restart an interrupted syscall instead of failing it with `EINTR`.
pub const SA_RESTART: u32 = 0x10000000;
