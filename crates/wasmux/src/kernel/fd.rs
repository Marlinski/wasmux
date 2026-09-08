//! Open file descriptions, pipes and descriptor tables.
//!
//! The shape follows Linux: a *descriptor* (an integer in a process's table) points at a
//! *description* (the thing with a file position and status flags), and `dup` makes a second
//! descriptor for the same description. Descriptions live in a slab owned by the kernel and
//! are referred to by key, so sharing them across processes costs a reference count rather
//! than an `Arc`, and nothing here needs interior mutability.
//!
//! Reference counts are adjusted explicitly, because a `Drop` impl cannot reach the slab.
//! Every path that ends a process goes through
//! [`Kernel::release_fds`](super::Kernel::release_fds).

use crate::abi::*;
use crate::errno::{Errno, VfsResult};
use crate::slab::{Key, Slab};
use crate::vfs::{FileType, VfsFile};
use std::collections::VecDeque;

/// A key into the kernel's description slab.
pub(crate) type DescKey = Key;
/// A key into the kernel's pipe slab.
pub(crate) type PipeKey = Key;

/// How many bytes a pipe holds before a writer blocks. Linux's default is the same.
pub(crate) const PIPE_CAPACITY: usize = 65536;

/// One directory entry as the guest will see it in `getdents64`.
pub(crate) struct DirEnt {
    pub(crate) ino: u64,
    pub(crate) kind: u8,
    pub(crate) name: String,
}

/// What an open description actually is.
pub(crate) enum DescKind {
    /// A file belonging to a mount.
    File {
        /// The mount's own handle.
        file: Box<dyn VfsFile>,
        /// Which mount it came from, so a write can be refused on a read-only one.
        mount: usize,
        /// Set once [`VfsFile::close`] has succeeded, so it is not called twice.
        closed: bool,
    },
    /// A directory, listed once at open time. `getdents64` walks the snapshot, which is what
    /// makes repeated reads cheap and a concurrent change invisible, as on a real filesystem
    /// with a cached directory stream.
    Dir { entries: Vec<DirEnt>, pos: usize },
    /// Bytes the kernel generated, such as a `/proc` file. Read-only and already complete.
    Memory { data: Vec<u8>, pos: usize },
    /// The reading end of a pipe.
    PipeRead(PipeKey),
    /// The writing end of a pipe.
    PipeWrite(PipeKey),
    /// The session's standard input.
    Stdin,
    /// The session's standard output (1) or error (2).
    Stdout(u8),
    /// `/dev/null`.
    Null,
    /// `/dev/zero`.
    Zero,
    /// `/dev/urandom`.
    Random,
}

/// An open file description: the state `dup` shares and `open` creates fresh.
pub(crate) struct Desc {
    pub(crate) kind: DescKind,
    /// The guest path it was opened as, for `/proc/self/fd` and `fchdir`.
    pub(crate) path: String,
    /// Access mode and status flags: `O_RDONLY`/`O_WRONLY`/`O_RDWR`, `O_APPEND`, `O_NONBLOCK`.
    pub(crate) flags: u32,
    /// Number of descriptors pointing here.
    pub(crate) refs: u32,
}

impl Desc {
    pub(crate) fn new(kind: DescKind, path: impl Into<String>, flags: u32) -> Desc {
        Desc {
            kind,
            path: path.into(),
            flags,
            refs: 1,
        }
    }

    pub(crate) fn is_nonblocking(&self) -> bool {
        self.flags & O_NONBLOCK != 0
    }

    /// Whether the guest may write, by the mode it opened with.
    pub(crate) fn writable(&self) -> bool {
        matches!(self.flags & O_ACCMODE, O_WRONLY | O_RDWR)
    }

    /// Whether the guest may read, by the mode it opened with.
    pub(crate) fn readable(&self) -> bool {
        matches!(self.flags & O_ACCMODE, O_RDONLY | O_RDWR)
    }

    /// Whether `isatty` should say yes.
    ///
    /// Always no. The session's standard streams are buffers the host owns, so a program that
    /// asks is told what is true: this is a pipe. That keeps `jq` from emitting colour codes
    /// and keeps a shell out of its line-editing path, which is what a tool wants.
    pub(crate) fn is_tty(&self) -> bool {
        false
    }
}

/// A pipe's buffer and the number of open ends.
pub(crate) struct Pipe {
    pub(crate) data: VecDeque<u8>,
    pub(crate) readers: u32,
    pub(crate) writers: u32,
}

impl Pipe {
    pub(crate) fn new() -> Pipe {
        Pipe {
            data: VecDeque::new(),
            readers: 1,
            writers: 1,
        }
    }

    pub(crate) fn room(&self) -> usize {
        PIPE_CAPACITY.saturating_sub(self.data.len())
    }
}

/// One descriptor in a process's table.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fd {
    pub(crate) desc: DescKey,
    pub(crate) cloexec: bool,
}

/// A process's descriptor table: the integers the guest passes around.
#[derive(Default)]
pub(crate) struct FdTable {
    slots: Vec<Option<Fd>>,
}

impl FdTable {
    pub(crate) fn new() -> FdTable {
        FdTable { slots: Vec::new() }
    }

    pub(crate) fn get(&self, fd: i32) -> VfsResult<Fd> {
        if fd < 0 {
            return Err(Errno::BADF);
        }
        self.slots
            .get(fd as usize)
            .and_then(|s| *s)
            .ok_or(Errno::BADF)
    }

    pub(crate) fn set_cloexec(&mut self, fd: i32, on: bool) -> VfsResult<()> {
        if fd < 0 {
            return Err(Errno::BADF);
        }
        match self.slots.get_mut(fd as usize).and_then(Option::as_mut) {
            Some(entry) => {
                entry.cloexec = on;
                Ok(())
            }
            None => Err(Errno::BADF),
        }
    }

    /// Put `entry` at the lowest free descriptor at or above `min`.
    pub(crate) fn alloc(&mut self, entry: Fd, min: usize, max: usize) -> VfsResult<i32> {
        let mut i = min;
        loop {
            if i >= max {
                return Err(Errno::MFILE);
            }
            if i >= self.slots.len() {
                self.slots.resize(i.saturating_add(1), None);
            }
            match self.slots.get_mut(i) {
                Some(slot) if slot.is_none() => {
                    *slot = Some(entry);
                    return Ok(i as i32);
                }
                Some(_) => i = i.saturating_add(1),
                None => return Err(Errno::MFILE),
            }
        }
    }

    /// Put `entry` at exactly `fd`, returning what was there.
    pub(crate) fn replace(&mut self, fd: i32, entry: Fd, max: usize) -> VfsResult<Option<Fd>> {
        if fd < 0 || fd as usize >= max {
            return Err(Errno::BADF);
        }
        let i = fd as usize;
        if i >= self.slots.len() {
            self.slots.resize(i.saturating_add(1), None);
        }
        match self.slots.get_mut(i) {
            Some(slot) => Ok(slot.replace(entry)),
            None => Err(Errno::BADF),
        }
    }

    /// Remove `fd`, returning the description it referred to.
    pub(crate) fn take(&mut self, fd: i32) -> VfsResult<Fd> {
        if fd < 0 {
            return Err(Errno::BADF);
        }
        match self.slots.get_mut(fd as usize) {
            Some(slot) => slot.take().ok_or(Errno::BADF),
            None => Err(Errno::BADF),
        }
    }

    /// Every live descriptor, for duplicating a table and for tearing a process down.
    fn entries(&self) -> impl Iterator<Item = (i32, Fd)> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|e| (i as i32, e)))
    }

    /// Drop the descriptors marked close-on-exec, returning their descriptions.
    pub(crate) fn drain_cloexec(&mut self) -> Vec<DescKey> {
        let mut released = Vec::new();
        for slot in self.slots.iter_mut() {
            if let Some(entry) = slot {
                if entry.cloexec {
                    released.push(entry.desc);
                    *slot = None;
                }
            }
        }
        released
    }

    /// Empty the table, returning every description that lost a reference.
    pub(crate) fn drain_all(&mut self) -> Vec<DescKey> {
        let mut released = Vec::new();
        for slot in self.slots.iter_mut() {
            if let Some(entry) = slot.take() {
                released.push(entry.desc);
            }
        }
        released
    }

    /// A copy for a new process. Every shared description gains a reference.
    pub(crate) fn duplicate(&self, descs: &mut Slab<Desc>) -> FdTable {
        for (_, entry) in self.entries() {
            if let Some(desc) = descs.get_mut(entry.desc) {
                desc.refs = desc.refs.saturating_add(1);
            }
        }
        FdTable {
            slots: self.slots.clone(),
        }
    }

    /// How many descriptors are open. Used by the tests and by `EMFILE` accounting.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}

/// The `d_type` byte for a directory entry.
pub(crate) fn dirent_kind(t: Option<FileType>) -> u8 {
    match t {
        Some(FileType::Dir) => DT_DIR,
        Some(FileType::File) => DT_REG,
        Some(FileType::Symlink) => DT_LNK,
        None => DT_UNKNOWN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_the_lowest_free_descriptor() {
        let mut t = FdTable::new();
        let e = Fd {
            desc: 0,
            cloexec: false,
        };
        assert_eq!(t.alloc(e, 0, 8), Ok(0));
        assert_eq!(t.alloc(e, 0, 8), Ok(1));
        assert_eq!(t.alloc(e, 0, 8), Ok(2));
        t.take(1).unwrap();
        assert_eq!(t.alloc(e, 0, 8), Ok(1), "the hole is filled first");
        assert_eq!(t.alloc(e, 5, 8), Ok(5), "a minimum is honoured");
    }

    #[test]
    fn enforces_the_descriptor_limit() {
        let mut t = FdTable::new();
        let e = Fd {
            desc: 0,
            cloexec: false,
        };
        for _ in 0..4 {
            t.alloc(e, 0, 4).unwrap();
        }
        assert_eq!(t.alloc(e, 0, 4), Err(Errno::MFILE));
        assert_eq!(t.replace(9, e, 4).err(), Some(Errno::BADF));
    }

    #[test]
    fn bad_descriptors_are_errors_not_panics() {
        let mut t = FdTable::new();
        assert_eq!(t.get(-1).err(), Some(Errno::BADF));
        assert_eq!(t.get(1000).err(), Some(Errno::BADF));
        assert_eq!(t.take(0).err(), Some(Errno::BADF));
        assert_eq!(t.set_cloexec(3, true).err(), Some(Errno::BADF));
    }

    #[test]
    fn cloexec_drains_only_marked_entries() {
        let mut t = FdTable::new();
        t.alloc(
            Fd {
                desc: 10,
                cloexec: false,
            },
            0,
            8,
        )
        .unwrap();
        t.alloc(
            Fd {
                desc: 11,
                cloexec: true,
            },
            0,
            8,
        )
        .unwrap();
        assert_eq!(t.drain_cloexec(), vec![11]);
        assert_eq!(t.count(), 1);
        assert_eq!(t.drain_all(), vec![10]);
        assert_eq!(t.count(), 0);
    }
}
