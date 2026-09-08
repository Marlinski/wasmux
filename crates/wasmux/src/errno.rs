//! Linux error numbers, as seen by the guest.
//!
//! [`Errno`] is the error type of the [`Vfs`](crate::Vfs) trait, so these are the values an
//! integrator returns. They are the guest's numbers, not the host's: a `Vfs` built on
//! `std::io` must translate, which [`Errno::from_io`] does when the `host-vfs` feature is on.

/// A Linux error number. Always positive; the kernel negates it at the syscall boundary.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Errno(pub i32);

macro_rules! errnos {
    ($($name:ident = $value:expr, $doc:literal;)*) => {
        impl Errno {
            $(#[doc = $doc] pub const $name: Errno = Errno($value);)*

            /// The conventional short name, e.g. `"ENOENT"`.
            pub fn name(self) -> &'static str {
                match self.0 {
                    $($value => concat!("E", stringify!($name)),)*
                    _ => "E?",
                }
            }
        }
    };
}

errnos! {
    PERM        =  1, "Operation not permitted.";
    NOENT       =  2, "No such file or directory.";
    SRCH        =  3, "No such process.";
    INTR        =  4, "Interrupted system call.";
    IO          =  5, "Input/output error.";
    NXIO        =  6, "No such device or address.";
    TOOBIG      =  7, "Argument list too long (`E2BIG`).";
    NOEXEC      =  8, "Exec format error.";
    BADF        =  9, "Bad file descriptor.";
    CHILD       = 10, "No child processes.";
    AGAIN       = 11, "Try again. From a `Vfs`, this means *not ready yet*; see the trait docs.";
    NOMEM       = 12, "Cannot allocate memory.";
    ACCES       = 13, "Permission denied.";
    FAULT       = 14, "Bad address.";
    BUSY        = 16, "Device or resource busy.";
    EXIST       = 17, "File exists.";
    XDEV        = 18, "Cross-device link.";
    NODEV       = 19, "No such device.";
    NOTDIR      = 20, "Not a directory.";
    ISDIR       = 21, "Is a directory.";
    INVAL       = 22, "Invalid argument.";
    NFILE       = 23, "Too many open files in system.";
    MFILE       = 24, "Too many open files.";
    NOTTY       = 25, "Not a typewriter.";
    FBIG        = 27, "File too large.";
    NOSPC       = 28, "No space left on device.";
    SPIPE       = 29, "Illegal seek.";
    ROFS        = 30, "Read-only file system.";
    MLINK       = 31, "Too many links.";
    PIPE        = 32, "Broken pipe.";
    RANGE       = 34, "Result too large, or a buffer too small.";
    NAMETOOLONG = 36, "File name too long.";
    NOSYS       = 38, "Function not implemented.";
    NOTEMPTY    = 39, "Directory not empty.";
    LOOP        = 40, "Too many levels of symbolic links.";
    OVERFLOW    = 75, "Value too large for defined data type.";
    NOTSOCK     = 88, "Socket operation on non-socket.";
    NOTSUP      = 95, "Operation not supported.";
    TIMEDOUT    = 110, "Connection timed out.";
}

impl Errno {
    /// The negative value a syscall returns for this error.
    pub const fn as_neg(self) -> i32 {
        self.0.wrapping_neg()
    }

    /// Translate a `std::io::Error` into the guest's numbering.
    ///
    /// Host and guest agree on Linux, but not on WASI, where the numbers are assigned in a
    /// different order entirely. Both are handled.
    #[cfg(feature = "host-vfs")]
    pub fn from_io(e: &std::io::Error) -> Errno {
        use std::io::ErrorKind as K;
        // Prefer the portable kind; fall back to the raw number only on Linux hosts, where it
        // is already the guest's numbering.
        match e.kind() {
            K::NotFound => Errno::NOENT,
            K::PermissionDenied => Errno::ACCES,
            K::AlreadyExists => Errno::EXIST,
            K::InvalidInput | K::InvalidData => Errno::INVAL,
            K::Unsupported => Errno::NOSYS,
            K::WouldBlock => Errno::AGAIN,
            K::BrokenPipe => Errno::PIPE,
            K::IsADirectory => Errno::ISDIR,
            K::NotADirectory => Errno::NOTDIR,
            K::DirectoryNotEmpty => Errno::NOTEMPTY,
            K::ReadOnlyFilesystem => Errno::ROFS,
            K::StorageFull => Errno::NOSPC,
            K::FileTooLarge => Errno::FBIG,
            K::TooManyLinks => Errno::MLINK,
            K::InvalidFilename => Errno::NAMETOOLONG,
            K::Interrupted => Errno::INTR,
            _ => {
                if cfg!(target_os = "linux") {
                    match e.raw_os_error() {
                        Some(n) if n > 0 => Errno(n),
                        _ => Errno::IO,
                    }
                } else {
                    Errno::IO
                }
            }
        }
    }
}

impl core::fmt::Debug for Errno {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} ({})", self.name(), self.0)
    }
}

impl core::fmt::Display for Errno {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for Errno {}

/// The result of a [`Vfs`](crate::Vfs) operation.
pub type VfsResult<T> = Result<T, Errno>;
