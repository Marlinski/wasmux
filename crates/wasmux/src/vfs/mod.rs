//! The filesystem the guest sees: implement [`Vfs`] to give it yours.
//!
//! # The contract
//!
//! **Paths** arrive absolute, normalized and relative to the mount point: a `Vfs` mounted at
//! `/work` is asked for `/notes/a.md` when the guest opens `/work/notes/a.md`. There are no
//! `.` or `..` components, no empty components and no trailing slash. The mount root is `"/"`.
//! A path can therefore be used as a key directly, with no sanitizing: the kernel has already
//! done it, and a guest cannot escape the mount.
//!
//! **Blocking is expressed by [`Errno::AGAIN`].** A `Vfs` backed by something asynchronous
//! (an object store, a database, another component) starts the work, returns `AGAIN`, and is
//! asked again later. The kernel suspends only the calling guest process; the rest keep
//! running, and [`Session::step`](crate::Session::step) reports
//! [`Wait::Vfs`](crate::Wait::Vfs) to the caller so it can await its own future and resume.
//! Three rules make that sound:
//!
//! 1. **Retries are identical.** The same call is repeated with the same arguments, so an
//!    implementation must be idempotent and may cache by argument.
//! 2. **Writes are all or nothing.** [`VfsFile::write`] either consumes bytes or returns
//!    `AGAIN` having consumed none. It must never both take bytes and report `AGAIN`.
//! 3. **`AGAIN` means progress is possible later.** Returning it forever is a hang; return a
//!    real error instead when the operation cannot succeed.
//!
//! A synchronous implementation never returns `AGAIN`, and then
//! [`Command::output`](crate::Command::output) can be used directly. [`MemVfs`] and
//! `HostVfs` (behind the `host-vfs` feature) are both of that kind.
//!
//! **What a `Vfs` is not asked for.** `/bin`, `/dev`, `/proc` and the programs are synthesized
//! by the kernel and never reach a mount. Neither do permissions checks: everything runs as
//! root inside the sandbox, and a read-only mount is enforced by the kernel, not the `Vfs`.
//!
//! # Example
//!
//! ```
//! use wasmux::{DirEntry, Errno, FileType, MemVfs, Stat, Vfs, VfsFile, VfsResult, OpenOptions};
//!
//! // Wrap another Vfs to make one path unreadable.
//! struct Censored<V>(V, &'static str);
//!
//! impl<V: Vfs> Vfs for Censored<V> {
//!     fn open(&self, path: &str, opts: &OpenOptions) -> VfsResult<Box<dyn VfsFile>> {
//!         if path == self.1 { return Err(Errno::ACCES); }
//!         self.0.open(path, opts)
//!     }
//!     fn stat(&self, path: &str, follow: bool) -> VfsResult<Stat> { self.0.stat(path, follow) }
//!     fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> { self.0.readdir(path) }
//! }
//!
//! let fs = Censored(MemVfs::new().with_file("/secret", b"..."), "/secret");
//! assert_eq!(fs.stat("/secret", true).map(|s| s.file_type), Ok(FileType::File));
//! assert!(fs.open("/secret", &OpenOptions::read()).is_err());
//! ```

use crate::errno::{Errno, VfsResult};

mod mem;
pub use mem::MemVfs;

#[cfg(feature = "host-vfs")]
mod host;
#[cfg(feature = "host-vfs")]
pub use host::HostVfs;

/// What a directory entry or a stat refers to.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub enum FileType {
    /// A regular file.
    #[default]
    File,
    /// A directory.
    Dir,
    /// A symbolic link.
    Symlink,
}

/// Metadata for one path.
///
/// Only [`Stat::file_type`] and [`Stat::size`] are needed; the rest have workable defaults, and
/// `mode` is completed by the kernel when left at zero.
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    /// File, directory or symlink.
    pub file_type: FileType,
    /// Size in bytes. Ignored for directories.
    pub size: u64,
    /// Permission bits, without the type bits. Zero means "use the default": `0o755` for a
    /// directory, `0o644` for a file, `0o777` for a symlink.
    pub mode: u32,
    /// Inode number, for tools that detect hard links or recursion. Zero means the kernel
    /// derives a stable one by hashing the path.
    pub ino: u64,
    /// Modification time in seconds since the epoch, and nanoseconds.
    pub mtime: (i64, u32),
    /// Link count. Zero is treated as one.
    pub nlink: u32,
}

impl Stat {
    /// A regular file of `size` bytes, with defaults for everything else.
    pub const fn file(size: u64) -> Stat {
        Stat {
            file_type: FileType::File,
            size,
            mode: 0,
            ino: 0,
            mtime: (0, 0),
            nlink: 1,
        }
    }
    /// A directory, with defaults for everything else.
    pub const fn dir() -> Stat {
        Stat {
            file_type: FileType::Dir,
            size: 0,
            mode: 0,
            ino: 0,
            mtime: (0, 0),
            nlink: 1,
        }
    }
    /// A symlink whose target is `len` bytes long.
    pub const fn symlink(len: u64) -> Stat {
        Stat {
            file_type: FileType::Symlink,
            size: len,
            mode: 0,
            ino: 0,
            mtime: (0, 0),
            nlink: 1,
        }
    }
}

/// One entry of a directory listing.
#[derive(Clone, Debug)]
pub struct DirEntry {
    /// The file name alone, with no slashes. `.` and `..` are added by the kernel.
    pub name: String,
    /// The kind of entry, if known. `None` costs the caller a `stat` when it needs the type.
    pub file_type: Option<FileType>,
    /// Inode number, or zero to let the kernel derive one.
    pub ino: u64,
}

impl DirEntry {
    /// An entry of known type.
    pub fn new(name: impl Into<String>, file_type: FileType) -> DirEntry {
        DirEntry {
            name: name.into(),
            file_type: Some(file_type),
            ino: 0,
        }
    }
}

/// Where the file position starts for a [`VfsFile::seek`].
#[derive(Clone, Copy, Debug)]
pub enum SeekFrom {
    /// From the beginning.
    Start(u64),
    /// From the end, backwards for a negative offset.
    End(i64),
    /// From the current position.
    Current(i64),
}

/// How the guest asked for a file to be opened.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenOptions {
    /// Readable.
    pub read: bool,
    /// Writable.
    pub write: bool,
    /// Every write goes to the end.
    pub append: bool,
    /// Create if missing.
    pub create: bool,
    /// Create, and fail with [`Errno::EXIST`] if it is already there.
    pub create_new: bool,
    /// Set the length to zero on open.
    pub truncate: bool,
    /// Permission bits for a newly created file.
    pub mode: u32,
}

impl OpenOptions {
    /// Read only, the common case.
    pub const fn read() -> OpenOptions {
        OpenOptions {
            read: true,
            ..OpenOptions::new()
        }
    }
    /// Create or truncate for writing.
    pub const fn write() -> OpenOptions {
        OpenOptions {
            write: true,
            create: true,
            truncate: true,
            mode: 0o644,
            ..OpenOptions::new()
        }
    }
    /// All flags off.
    pub const fn new() -> OpenOptions {
        OpenOptions {
            read: false,
            write: false,
            append: false,
            create: false,
            create_new: false,
            truncate: false,
            mode: 0o644,
        }
    }
}

/// A filesystem mounted into the guest's namespace.
///
/// See the [module documentation](self) for the path, blocking and idempotence rules. Only
/// three methods are required; the rest default to a read-only filesystem.
pub trait Vfs: Send + Sync + 'static {
    /// Open an existing file, or create one when [`OpenOptions::create`] is set.
    ///
    /// Opening a directory is the kernel's business, not this method's: it calls
    /// [`Vfs::readdir`] instead. A directory path here may return [`Errno::ISDIR`].
    fn open(&self, path: &str, opts: &OpenOptions) -> VfsResult<Box<dyn VfsFile>>;

    /// Metadata for `path`. With `follow` false, describe a symlink itself rather than its
    /// target; an implementation without symlinks can ignore the flag.
    fn stat(&self, path: &str, follow: bool) -> VfsResult<Stat>;

    /// List a directory, in any order, without `.` or `..`.
    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>>;

    /// Create a directory. The default refuses with [`Errno::ROFS`].
    fn mkdir(&self, path: &str, mode: u32) -> VfsResult<()> {
        let _ = (path, mode);
        Err(Errno::ROFS)
    }

    /// Remove a file. The default refuses with [`Errno::ROFS`].
    fn unlink(&self, path: &str) -> VfsResult<()> {
        let _ = path;
        Err(Errno::ROFS)
    }

    /// Remove an empty directory. The default refuses with [`Errno::ROFS`].
    fn rmdir(&self, path: &str) -> VfsResult<()> {
        let _ = path;
        Err(Errno::ROFS)
    }

    /// Rename within this filesystem. The default refuses with [`Errno::ROFS`].
    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        let _ = (from, to);
        Err(Errno::ROFS)
    }

    /// Create a symlink at `link` pointing at `target`, which is uninterpreted. The default
    /// refuses with [`Errno::PERM`], which is what a filesystem without symlinks reports.
    fn symlink(&self, target: &str, link: &str) -> VfsResult<()> {
        let _ = (target, link);
        Err(Errno::PERM)
    }

    /// Read a symlink's target. The default reports [`Errno::INVAL`], which is what Linux
    /// returns for a path that is not a symlink.
    fn readlink(&self, path: &str) -> VfsResult<String> {
        let _ = path;
        Err(Errno::INVAL)
    }

    /// Set the length of a file by path. The default opens it and calls
    /// [`VfsFile::set_len`].
    fn truncate(&self, path: &str, len: u64) -> VfsResult<()> {
        let mut f = self.open(
            path,
            &OpenOptions {
                write: true,
                ..OpenOptions::new()
            },
        )?;
        f.set_len(len)?;
        f.close()
    }

    /// Whether this filesystem has symlinks at all.
    ///
    /// The default is `false`, and then the kernel never probes for them: it saves a `stat`
    /// per path component, which is the difference between one round trip and several on a
    /// filesystem that lives across a network. Say `true` only if [`Vfs::readlink`] can
    /// return something.
    fn has_symlinks(&self) -> bool {
        false
    }

    /// Change permission bits. The default accepts and ignores, because the sandbox has one
    /// user and the kernel enforces read-only mounts itself.
    fn set_mode(&self, path: &str, mode: u32) -> VfsResult<()> {
        let _ = (path, mode);
        Ok(())
    }
}

/// An open file belonging to a [`Vfs`].
///
/// Dropping one is not a failure path: the kernel calls [`VfsFile::close`] first and reports
/// what it returns, so that a store which uploads on close can say so.
pub trait VfsFile: Send {
    /// Read at the current position, advancing it. Zero means end of file.
    fn read(&mut self, buf: &mut [u8]) -> VfsResult<usize>;

    /// Write at the current position, advancing it. A short write is allowed; returning
    /// [`Errno::AGAIN`] is only allowed when nothing was consumed.
    fn write(&mut self, data: &[u8]) -> VfsResult<usize>;

    /// Move the file position, returning the new absolute position.
    fn seek(&mut self, from: SeekFrom) -> VfsResult<u64>;

    /// Metadata for the open file.
    fn stat(&self) -> VfsResult<Stat>;

    /// Set the length, zero-filling if it grows. The default refuses with [`Errno::ROFS`].
    fn set_len(&mut self, len: u64) -> VfsResult<()> {
        let _ = len;
        Err(Errno::ROFS)
    }

    /// Flush and release. Called exactly once, before the handle is dropped, and may return
    /// [`Errno::AGAIN`] to be called again.
    fn close(&mut self) -> VfsResult<()> {
        Ok(())
    }
}

/// A `Vfs` that answers every call with [`Errno::ROFS`] or [`Errno::NOENT`].
///
/// Useful as a placeholder mount, and as the thing a guest sees at a path the host declines to
/// back with anything.
pub struct EmptyVfs;

impl Vfs for EmptyVfs {
    fn open(&self, _path: &str, _opts: &OpenOptions) -> VfsResult<Box<dyn VfsFile>> {
        Err(Errno::NOENT)
    }
    fn stat(&self, path: &str, _follow: bool) -> VfsResult<Stat> {
        if path == "/" {
            Ok(Stat::dir())
        } else {
            Err(Errno::NOENT)
        }
    }
    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        if path == "/" {
            Ok(Vec::new())
        } else {
            Err(Errno::NOENT)
        }
    }
}
