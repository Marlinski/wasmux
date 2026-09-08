//! An in-memory [`Vfs`]: the default for `/tmp`, and what the test suite runs on.

use super::{DirEntry, FileType, OpenOptions, SeekFrom, Stat, Vfs, VfsFile};
use crate::errno::{Errno, VfsResult};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

type Bytes = Arc<Mutex<Vec<u8>>>;

enum Node {
    Dir(BTreeMap<String, Node>),
    File(Bytes),
    Link(String),
}

/// A filesystem held in memory, shareable and cheap.
///
/// Fully synchronous: it never returns [`Errno::AGAIN`], so it works with
/// [`Command::output`](crate::Command::output).
///
/// ```
/// use wasmux::{MemVfs, Vfs};
/// let fs = MemVfs::new().with_file("/a/b.txt", b"hello");
/// assert_eq!(fs.stat("/a", true).unwrap().file_type, wasmux::FileType::Dir);
/// assert_eq!(fs.read_file("/a/b.txt").unwrap(), b"hello");
/// ```
pub struct MemVfs {
    root: Mutex<Node>,
}

impl Default for MemVfs {
    fn default() -> Self {
        Self::new()
    }
}

/// Split a validated path into components. `/` yields nothing.
fn parts(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|c| !c.is_empty())
}

fn split_parent(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(0) => ("/", path.get(1..).unwrap_or("")),
        Some(i) => (
            path.get(..i).unwrap_or("/"),
            path.get(i.saturating_add(1)..).unwrap_or(""),
        ),
        None => ("/", path),
    }
}

impl Node {
    fn find(&self, path: &str) -> Option<&Node> {
        let mut cur = self;
        for name in parts(path) {
            match cur {
                Node::Dir(entries) => cur = entries.get(name)?,
                _ => return None,
            }
        }
        Some(cur)
    }

    fn find_mut(&mut self, path: &str) -> Option<&mut Node> {
        let mut cur = self;
        for name in parts(path) {
            match cur {
                Node::Dir(entries) => cur = entries.get_mut(name)?,
                _ => return None,
            }
        }
        Some(cur)
    }

    /// The directory map at `path`, creating nothing.
    fn dir_mut(&mut self, path: &str) -> VfsResult<&mut BTreeMap<String, Node>> {
        match self.find_mut(path) {
            Some(Node::Dir(entries)) => Ok(entries),
            Some(_) => Err(Errno::NOTDIR),
            None => Err(Errno::NOENT),
        }
    }

    /// Create every missing directory along `path`.
    fn make_dirs(&mut self, path: &str) {
        let mut cur = self;
        for name in parts(path) {
            let entries = match cur {
                Node::Dir(entries) => entries,
                _ => return,
            };
            cur = entries
                .entry(name.to_string())
                .or_insert_with(|| Node::Dir(BTreeMap::new()));
        }
    }

    fn stat(&self) -> Stat {
        match self {
            Node::Dir(_) => Stat::dir(),
            Node::File(b) => Stat::file(b.lock().map(|d| d.len()).unwrap_or(0) as u64),
            Node::Link(t) => Stat::symlink(t.len() as u64),
        }
    }
}

impl MemVfs {
    /// An empty filesystem with just a root directory.
    pub fn new() -> MemVfs {
        MemVfs {
            root: Mutex::new(Node::Dir(BTreeMap::new())),
        }
    }

    /// Builder form of [`MemVfs::write_file`], creating parent directories.
    pub fn with_file(self, path: &str, contents: impl Into<Vec<u8>>) -> MemVfs {
        let _ = self.write_file(path, contents);
        self
    }

    /// Builder form of [`MemVfs::mkdir_all`].
    pub fn with_dir(self, path: &str) -> MemVfs {
        let _ = self.mkdir_all(path);
        self
    }

    /// Create `path` and every parent directory.
    pub fn mkdir_all(&self, path: &str) -> VfsResult<()> {
        let mut root = self.root.lock().map_err(|_| Errno::IO)?;
        root.make_dirs(path);
        Ok(())
    }

    /// Replace the contents of `path`, creating parent directories as needed.
    pub fn write_file(&self, path: &str, contents: impl Into<Vec<u8>>) -> VfsResult<()> {
        let (parent, name) = split_parent(path);
        if name.is_empty() {
            return Err(Errno::ISDIR);
        }
        let mut root = self.root.lock().map_err(|_| Errno::IO)?;
        root.make_dirs(parent);
        let entries = root.dir_mut(parent)?;
        match entries.get(name) {
            // Keep the identity of an existing file so open handles see the change.
            Some(Node::File(b)) => {
                let mut data = b.lock().map_err(|_| Errno::IO)?;
                *data = contents.into();
            }
            Some(_) => return Err(Errno::ISDIR),
            None => {
                entries.insert(
                    name.to_string(),
                    Node::File(Arc::new(Mutex::new(contents.into()))),
                );
            }
        }
        Ok(())
    }

    /// The whole contents of `path`.
    pub fn read_file(&self, path: &str) -> VfsResult<Vec<u8>> {
        let root = self.root.lock().map_err(|_| Errno::IO)?;
        match root.find(path) {
            Some(Node::File(b)) => Ok(b.lock().map_err(|_| Errno::IO)?.clone()),
            Some(Node::Dir(_)) => Err(Errno::ISDIR),
            Some(Node::Link(_)) | None => Err(Errno::NOENT),
        }
    }

    /// Every file in the tree as `(path, contents)`, sorted. For assertions in tests.
    pub fn snapshot(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        if let Ok(root) = self.root.lock() {
            collect(&root, "", &mut out);
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

fn collect(node: &Node, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) {
    match node {
        Node::Dir(entries) => {
            for (name, child) in entries {
                let path = format!("{prefix}/{name}");
                collect(child, &path, out);
            }
        }
        Node::File(b) => {
            if let Ok(data) = b.lock() {
                out.push((prefix.to_string(), data.clone()));
            }
        }
        Node::Link(target) => out.push((prefix.to_string(), target.as_bytes().to_vec())),
    }
}

impl Vfs for MemVfs {
    fn has_symlinks(&self) -> bool {
        true
    }

    fn open(&self, path: &str, opts: &OpenOptions) -> VfsResult<Box<dyn VfsFile>> {
        let (parent, name) = split_parent(path);
        if name.is_empty() {
            return Err(Errno::ISDIR);
        }
        let mut root = self.root.lock().map_err(|_| Errno::IO)?;
        let existing = match root.find(path) {
            Some(Node::File(b)) => Some(b.clone()),
            Some(Node::Dir(_)) => return Err(Errno::ISDIR),
            Some(Node::Link(_)) => return Err(Errno::LOOP),
            None => None,
        };
        let data = match existing {
            Some(b) => {
                if opts.create_new {
                    return Err(Errno::EXIST);
                }
                if opts.truncate && opts.write {
                    b.lock().map_err(|_| Errno::IO)?.clear();
                }
                b
            }
            None => {
                if !opts.create && !opts.create_new {
                    return Err(Errno::NOENT);
                }
                let b: Bytes = Arc::new(Mutex::new(Vec::new()));
                let entries = root.dir_mut(parent)?;
                entries.insert(name.to_string(), Node::File(b.clone()));
                b
            }
        };
        let pos = if opts.append {
            data.lock().map_err(|_| Errno::IO)?.len() as u64
        } else {
            0
        };
        Ok(Box::new(MemFile {
            data,
            pos,
            append: opts.append,
            writable: opts.write || opts.append,
        }))
    }

    fn stat(&self, path: &str, follow: bool) -> VfsResult<Stat> {
        let root = self.root.lock().map_err(|_| Errno::IO)?;
        let node = root.find(path).ok_or(Errno::NOENT)?;
        match node {
            Node::Link(target) if follow => {
                let target = target.clone();
                let resolved = if target.starts_with('/') {
                    target
                } else {
                    let (parent, _) = split_parent(path);
                    format!("{}/{}", parent.trim_end_matches('/'), target)
                };
                root.find(&resolved).map(Node::stat).ok_or(Errno::NOENT)
            }
            other => Ok(other.stat()),
        }
    }

    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let root = self.root.lock().map_err(|_| Errno::IO)?;
        match root.find(path) {
            Some(Node::Dir(entries)) => Ok(entries
                .iter()
                .map(|(name, node)| DirEntry {
                    name: name.clone(),
                    file_type: Some(match node {
                        Node::Dir(_) => FileType::Dir,
                        Node::File(_) => FileType::File,
                        Node::Link(_) => FileType::Symlink,
                    }),
                    ino: 0,
                })
                .collect()),
            Some(_) => Err(Errno::NOTDIR),
            None => Err(Errno::NOENT),
        }
    }

    fn mkdir(&self, path: &str, _mode: u32) -> VfsResult<()> {
        let (parent, name) = split_parent(path);
        if name.is_empty() {
            return Err(Errno::EXIST);
        }
        let mut root = self.root.lock().map_err(|_| Errno::IO)?;
        if root.find(path).is_some() {
            return Err(Errno::EXIST);
        }
        let entries = root.dir_mut(parent)?;
        entries.insert(name.to_string(), Node::Dir(BTreeMap::new()));
        Ok(())
    }

    fn unlink(&self, path: &str) -> VfsResult<()> {
        let (parent, name) = split_parent(path);
        let mut root = self.root.lock().map_err(|_| Errno::IO)?;
        let entries = root.dir_mut(parent)?;
        match entries.get(name) {
            Some(Node::Dir(_)) => Err(Errno::ISDIR),
            Some(_) => {
                entries.remove(name);
                Ok(())
            }
            None => Err(Errno::NOENT),
        }
    }

    fn rmdir(&self, path: &str) -> VfsResult<()> {
        let (parent, name) = split_parent(path);
        let mut root = self.root.lock().map_err(|_| Errno::IO)?;
        let entries = root.dir_mut(parent)?;
        match entries.get(name) {
            Some(Node::Dir(inner)) if inner.is_empty() => {
                entries.remove(name);
                Ok(())
            }
            Some(Node::Dir(_)) => Err(Errno::NOTEMPTY),
            Some(_) => Err(Errno::NOTDIR),
            None => Err(Errno::NOENT),
        }
    }

    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        let (from_parent, from_name) = split_parent(from);
        let (to_parent, to_name) = split_parent(to);
        if from_name.is_empty() || to_name.is_empty() {
            return Err(Errno::INVAL);
        }
        let mut root = self.root.lock().map_err(|_| Errno::IO)?;
        let node = {
            let entries = root.dir_mut(from_parent)?;
            entries.remove(from_name).ok_or(Errno::NOENT)?
        };
        match root.dir_mut(to_parent) {
            Ok(entries) => {
                entries.insert(to_name.to_string(), node);
                Ok(())
            }
            Err(e) => {
                // Put it back rather than lose it.
                if let Ok(entries) = root.dir_mut(from_parent) {
                    entries.insert(from_name.to_string(), node);
                }
                Err(e)
            }
        }
    }

    fn symlink(&self, target: &str, link: &str) -> VfsResult<()> {
        let (parent, name) = split_parent(link);
        if name.is_empty() {
            return Err(Errno::EXIST);
        }
        let mut root = self.root.lock().map_err(|_| Errno::IO)?;
        if root.find(link).is_some() {
            return Err(Errno::EXIST);
        }
        let entries = root.dir_mut(parent)?;
        entries.insert(name.to_string(), Node::Link(target.to_string()));
        Ok(())
    }

    fn readlink(&self, path: &str) -> VfsResult<String> {
        let root = self.root.lock().map_err(|_| Errno::IO)?;
        match root.find(path) {
            Some(Node::Link(target)) => Ok(target.clone()),
            Some(_) => Err(Errno::INVAL),
            None => Err(Errno::NOENT),
        }
    }
}

struct MemFile {
    data: Bytes,
    pos: u64,
    append: bool,
    writable: bool,
}

impl VfsFile for MemFile {
    fn read(&mut self, buf: &mut [u8]) -> VfsResult<usize> {
        let data = self.data.lock().map_err(|_| Errno::IO)?;
        let start = self.pos.min(data.len() as u64) as usize;
        let src = data.get(start..).unwrap_or(&[]);
        let n = src.len().min(buf.len());
        match (buf.get_mut(..n), src.get(..n)) {
            (Some(dst), Some(src)) => dst.copy_from_slice(src),
            _ => return Ok(0),
        }
        self.pos = self.pos.saturating_add(n as u64);
        Ok(n)
    }

    fn write(&mut self, bytes: &[u8]) -> VfsResult<usize> {
        if !self.writable {
            return Err(Errno::BADF);
        }
        let mut data = self.data.lock().map_err(|_| Errno::IO)?;
        if self.append {
            self.pos = data.len() as u64;
        }
        let start = self.pos as usize;
        let end = start.saturating_add(bytes.len());
        if data.len() < end {
            data.resize(end, 0);
        }
        match data.get_mut(start..end) {
            Some(dst) => dst.copy_from_slice(bytes),
            None => return Err(Errno::IO),
        }
        self.pos = end as u64;
        Ok(bytes.len())
    }

    fn seek(&mut self, from: SeekFrom) -> VfsResult<u64> {
        let len = self.data.lock().map_err(|_| Errno::IO)?.len() as i64;
        let target = match from {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(d) => len.saturating_add(d),
            SeekFrom::Current(d) => (self.pos as i64).saturating_add(d),
        };
        if target < 0 {
            return Err(Errno::INVAL);
        }
        self.pos = target as u64;
        Ok(self.pos)
    }

    fn stat(&self) -> VfsResult<Stat> {
        Ok(Stat::file(
            self.data.lock().map_err(|_| Errno::IO)?.len() as u64
        ))
    }

    fn set_len(&mut self, len: u64) -> VfsResult<()> {
        if !self.writable {
            return Err(Errno::BADF);
        }
        self.data
            .lock()
            .map_err(|_| Errno::IO)?
            .resize(len as usize, 0);
        Ok(())
    }
}
