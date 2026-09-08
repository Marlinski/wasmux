//! A [`Vfs`] over a directory of the host filesystem. Requires the `host-vfs` feature.
//!
//! For the CLI, the test suite and native embeddings. A guest component has no host
//! filesystem, so this is not the mount an agent uses.

use super::{DirEntry, FileType, OpenOptions, SeekFrom, Stat, Vfs, VfsFile};
use crate::errno::{Errno, VfsResult};
use std::fs;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

/// Exposes `root` and everything under it.
///
/// Paths from the kernel are already normalized, so no `..` can arrive here; the join is
/// nonetheless component-wise and rejects anything that is not a plain name, as defence in
/// depth against a bug elsewhere.
pub struct HostVfs {
    root: PathBuf,
}

impl HostVfs {
    /// Expose `root`, which must already exist.
    pub fn new(root: impl Into<PathBuf>) -> VfsResult<HostVfs> {
        let root = root.into();
        let meta = fs::metadata(&root).map_err(|e| Errno::from_io(&e))?;
        if !meta.is_dir() {
            return Err(Errno::NOTDIR);
        }
        Ok(HostVfs { root })
    }

    fn resolve(&self, path: &str) -> VfsResult<PathBuf> {
        let mut out = self.root.clone();
        for component in path.split('/').filter(|c| !c.is_empty()) {
            if component == "." || component == ".." || component.contains('\0') {
                return Err(Errno::INVAL);
            }
            out.push(component);
        }
        Ok(out)
    }
}

fn stat_of(meta: &fs::Metadata, path: &Path) -> Stat {
    let file_type = if meta.is_dir() {
        FileType::Dir
    } else if meta.file_type().is_symlink() {
        FileType::Symlink
    } else {
        FileType::File
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| (d.as_secs() as i64, d.subsec_nanos()))
        .unwrap_or((0, 0));
    let mode = mode_of(meta);
    Stat {
        file_type,
        size: meta.len(),
        mode,
        ino: ino_of(meta, path),
        mtime,
        nlink: 1,
    }
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode() & 0o7777
}
#[cfg(not(unix))]
fn mode_of(meta: &fs::Metadata) -> u32 {
    // WASI reports no permission bits. Zero lets the kernel apply its defaults.
    let _ = meta;
    0
}

#[cfg(unix)]
fn ino_of(meta: &fs::Metadata, _path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}
#[cfg(not(unix))]
fn ino_of(_meta: &fs::Metadata, _path: &Path) -> u64 {
    0 // the kernel hashes the path instead
}

impl Vfs for HostVfs {
    fn has_symlinks(&self) -> bool {
        true
    }

    fn open(&self, path: &str, opts: &OpenOptions) -> VfsResult<Box<dyn VfsFile>> {
        let full = self.resolve(path)?;
        let mut oo = fs::OpenOptions::new();
        oo.read(opts.read)
            .write(opts.write)
            .append(opts.append)
            .truncate(opts.truncate && opts.write);
        if opts.create_new {
            oo.create_new(true);
        } else if opts.create {
            oo.create(true);
        }
        set_mode_on_open(&mut oo, opts.mode);
        let file = oo.open(&full).map_err(|e| Errno::from_io(&e))?;
        Ok(Box::new(HostFile { file }))
    }

    fn stat(&self, path: &str, follow: bool) -> VfsResult<Stat> {
        let full = self.resolve(path)?;
        let meta = if follow {
            fs::metadata(&full)
        } else {
            fs::symlink_metadata(&full)
        }
        .map_err(|e| Errno::from_io(&e))?;
        Ok(stat_of(&meta, &full))
    }

    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let full = self.resolve(path)?;
        let mut out = Vec::new();
        for entry in fs::read_dir(&full).map_err(|e| Errno::from_io(&e))? {
            let entry = entry.map_err(|e| Errno::from_io(&e))?;
            let file_type = entry.file_type().ok().map(|t| {
                if t.is_dir() {
                    FileType::Dir
                } else if t.is_symlink() {
                    FileType::Symlink
                } else {
                    FileType::File
                }
            });
            out.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                file_type,
                ino: entry
                    .metadata()
                    .map(|m| ino_of(&m, &entry.path()))
                    .unwrap_or(0),
            });
        }
        Ok(out)
    }

    fn mkdir(&self, path: &str, _mode: u32) -> VfsResult<()> {
        fs::create_dir(self.resolve(path)?).map_err(|e| Errno::from_io(&e))
    }

    fn unlink(&self, path: &str) -> VfsResult<()> {
        fs::remove_file(self.resolve(path)?).map_err(|e| Errno::from_io(&e))
    }

    fn rmdir(&self, path: &str) -> VfsResult<()> {
        fs::remove_dir(self.resolve(path)?).map_err(|e| Errno::from_io(&e))
    }

    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        fs::rename(self.resolve(from)?, self.resolve(to)?).map_err(|e| Errno::from_io(&e))
    }

    fn symlink(&self, target: &str, link: &str) -> VfsResult<()> {
        let link = self.resolve(link)?;
        symlink_impl(target, &link)
    }

    fn readlink(&self, path: &str) -> VfsResult<String> {
        let full = self.resolve(path)?;
        let target = fs::read_link(&full).map_err(|e| Errno::from_io(&e))?;
        Ok(target.to_string_lossy().into_owned())
    }

    fn set_mode(&self, path: &str, mode: u32) -> VfsResult<()> {
        set_mode_impl(&self.resolve(path)?, mode)
    }
}

#[cfg(unix)]
fn set_mode_on_open(oo: &mut fs::OpenOptions, mode: u32) {
    use std::os::unix::fs::OpenOptionsExt;
    oo.mode(mode);
}
#[cfg(not(unix))]
fn set_mode_on_open(_oo: &mut fs::OpenOptions, _mode: u32) {}

#[cfg(unix)]
fn set_mode_impl(path: &Path, mode: u32) -> VfsResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| Errno::from_io(&e))
}
#[cfg(not(unix))]
fn set_mode_impl(_path: &Path, _mode: u32) -> VfsResult<()> {
    Ok(()) // WASI has no permission bits
}

#[cfg(unix)]
fn symlink_impl(target: &str, link: &Path) -> VfsResult<()> {
    std::os::unix::fs::symlink(target, link).map_err(|e| Errno::from_io(&e))
}
#[cfg(not(unix))]
fn symlink_impl(_target: &str, _link: &Path) -> VfsResult<()> {
    Err(Errno::PERM)
}

struct HostFile {
    file: fs::File,
}

impl VfsFile for HostFile {
    fn read(&mut self, buf: &mut [u8]) -> VfsResult<usize> {
        self.file.read(buf).map_err(|e| Errno::from_io(&e))
    }
    fn write(&mut self, data: &[u8]) -> VfsResult<usize> {
        self.file.write(data).map_err(|e| Errno::from_io(&e))
    }
    fn seek(&mut self, from: SeekFrom) -> VfsResult<u64> {
        let target = match from {
            SeekFrom::Start(p) => std::io::SeekFrom::Start(p),
            SeekFrom::End(d) => std::io::SeekFrom::End(d),
            SeekFrom::Current(d) => std::io::SeekFrom::Current(d),
        };
        self.file.seek(target).map_err(|e| Errno::from_io(&e))
    }
    fn stat(&self) -> VfsResult<Stat> {
        let meta = self.file.metadata().map_err(|e| Errno::from_io(&e))?;
        Ok(stat_of(&meta, Path::new("")))
    }
    fn set_len(&mut self, len: u64) -> VfsResult<()> {
        self.file.set_len(len).map_err(|e| Errno::from_io(&e))
    }
    fn close(&mut self) -> VfsResult<()> {
        self.file.flush().map_err(|e| Errno::from_io(&e))
    }
}
